//! The modal surfaces: the connection form, the palette, the confirmations.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use std::path::Path;
use std::time::Duration;

use gpui::{Animation, AnimationExt, Div, PathPromptOptions, StyledText, ease_out_quint};
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::Textarea;
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Sizable};

use super::agent::{approval_facts, visibility_facts};
use super::folders::{DragLabel, DragShortcut};
use super::welcome::{self, Part, Pose};
use super::*;
use crate::actions::{ConfirmDialog, NextDialogButton, PreviousDialogButton, TickDialogBox};
use crate::agent::result::Total;
use crate::connection_form::{ConnectionTest, Reported, dialects, suggested_dialect};
use crate::explorer::{
    ExplorerLeaf, FILES_GROUP, FilesOrder, Landing, Member, PINNED_GROUP, Pin, Shortcut,
    ShortcutDatabase, Shortcuts, is_schema_row, landing, relation_copies, saved_query,
};
use crate::icons::{queries_group_icon, saved_query_icon};
use crate::scroller::{SmoothScrollable, smooth_scoped};
use crate::session::{Depth, PendingDelete};
use crate::sidebar::{NodeAction, NodeKind, SelectedNode, SelectionAction, node_actions};
use crate::sql::{Destructive, Stop};
use crate::ui::{
    Status, button_inked, code_lines, code_lines_visible, dot, raised_card, row_icon_tinted,
    size_fact,
};

impl Workspace {
    pub(crate) fn render_connection_form(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = popup_theme(cx);
        let form = self
            .form
            .as_ref()
            .expect("form is rendered only while open");
        // Scoped to the profile being edited, or "new", so switching from
        // editing one profile to another (or to a fresh connection) doesn't
        // open on the last profile's scroll offset.
        let scope = form.editing.as_deref().unwrap_or("new");
        let viewport = window.viewport_size();
        let width = layout::FORM_WIDTH
            .min(viewport.width.as_f32() - 2. * layout::SPACE_LG)
            .max(0.);
        let tallest =
            (viewport.height.as_f32() - layout::TITLEBAR_HEIGHT - 2. * layout::FORM_TOP).max(0.);
        let elapsed = if cx.reduce_motion() {
            f32::INFINITY
        } else {
            form.opened.elapsed().as_secs_f32() * 1000.
        };
        if elapsed < welcome::form_entrance_ms(form.backdrop_shown) {
            window.request_animation_frame();
        }
        let backdrop = if form.backdrop_shown {
            Pose {
                opacity: 1.,
                offset: 0.,
            }
        } else {
            welcome::pose(Part::Backdrop, false, elapsed)
        };

        div()
            .relative()
            .size_full()
            .children(welcome::backdrop(*theme(cx), viewport, backdrop))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(layout::FORM_TOP))
                    .child(
                        welcome::posed(div().relative(), welcome::pose(Part::Form, false, elapsed))
                            .flex_shrink_0()
                            .w(px(width))
                            .max_h(px(tallest))
                            .flex()
                            .flex_col()
                            .text_size(px(layout::chrome(layout::TEXT_MD)))
                            .text_color(t.text)
                            .border_1()
                            .border_color(t.border_strong)
                            .rounded(px(layout::RADIUS_CARD))
                            .map(|card| ui::raised_card(card, t))
                            .overflow_hidden()
                            .child(self.render_form_header(t, cx))
                            // The header and the footer stay put; only the
                            // sections scroll, once the window is too short for
                            // them.
                            .child(
                                div()
                                    .id("connection-form-scroll")
                                    .flex_shrink(1.)
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .smooth_scroll(&smooth_scoped(
                                        "connection-form-scroll",
                                        scope,
                                        cx,
                                    ))
                                    .child(self.render_form_sections(t, cx)),
                            )
                            .child(self.render_form_footer(t, cx)),
                    ),
            )
    }

    /// The engine's logo, what the connection is called, where it points, and
    /// the way to another engine or out.
    fn render_form_header(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        let target = form.target();
        let brand = theme::engine_brand(target.engine, t, &t.logo_planes());
        let title = match form.name.read(cx).value().trim() {
            "" if form.editing.is_some() => "Edit connection".to_string(),
            "" => "New connection".to_string(),
            name => name.to_string(),
        };
        let mono = fonts(cx).family(FontSlot::Editor).clone();
        let close = match ui::chord_hint("show_editor", &self.settings.custom_keybindings) {
            hint if hint.is_empty() => "Close".to_string(),
            hint => format!("Close ({hint})"),
        };
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_MD))
            .pl(px(layout::FORM_PADDING))
            .pr(px(layout::SPACE_LG))
            .py(px(layout::SPACE_LG))
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(layout::chrome(layout::FORM_LOGO_TILE)))
                    .rounded(px(layout::RADIUS_PANEL))
                    .bg(brand.alpha(layout::ACCENT_WASH))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        icon(target.logo())
                            .size(px(layout::chrome(layout::FORM_LOGO_SIZE)))
                            .text_color(brand),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(layout::SETTINGS_DESCRIPTION_GAP))
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(layout::chrome(layout::FORM_TITLE_SIZE)))
                            .line_height(px(layout::chrome(layout::FORM_TITLE_LINE)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .children(form.summary(cx).map(|summary| {
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(mono)
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text_faint)
                            .child(summary)
                    })),
            )
            // The engine decides which fields the form even has. There are
            // too many to scan in a menu, so it opens the searchable palette.
            .child(
                ui::control("form-engine", Tone::Quiet, Control::Standard)
                    .px(px(layout::SPACE_SM))
                    .border_1()
                    .border_color(t.border)
                    .tooltip("Change engine")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .text_size(px(layout::chrome(layout::TEXT_MD)))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(t.text)
                            .child(
                                icon(target.logo())
                                    .size(px(layout::chrome(layout::ICON_SIZE)))
                                    .text_color(brand),
                            )
                            .child(target.label())
                            .child(
                                icon(icon::CHOOSE)
                                    .size(px(layout::chrome(layout::ICON_SIZE)))
                                    .text_color(t.text_faint),
                            ),
                    )
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.open_palette(PaletteMode::Engine, window, cx);
                    })),
            )
            .child(
                icon_button("close-form", icon::CLOSE, Tone::Quiet, Control::Standard, t)
                    .tooltip(close)
                    .on_click(cx.listener(|workspace, _, _, cx| workspace.close_form(cx))),
            )
    }

    /// Where the connection goes, how it gets there safely, and what it may
    /// do once there: each a well of rows.
    fn render_form_sections(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        let mono = fonts(cx).family(FontSlot::Editor).clone();
        let mono_field = |input: &Entity<InputState>| bare(input).font_family(mono.clone());
        let fields = form.target().fields();

        let mut connection = Vec::new();
        // Snowflake has no connection URL, and a file is its own field:
        // `Target::fields()` is where that is decided, not a match on the
        // engine here.
        if fields == Fields::Server {
            connection.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .min_h(px(layout::chrome(layout::FORM_ROW_HEIGHT)))
                    .px(px(layout::SPACE_LG))
                    .child(
                        icon(icon::LINK)
                            .size(px(layout::chrome(layout::ICON_SIZE)))
                            .text_color(t.text_faint),
                    )
                    .child(div().flex_1().min_w_0().child(mono_field(&form.url)))
                    .into_any_element(),
            );
        }
        connection
            .push(field_row("name", "Name", &form.name, bare(&form.name), t).into_any_element());
        match fields {
            // An engine that is a file has no host, no credentials and no
            // transport, so those fields are absent rather than present and
            // inert. A disabled field still reads as something the
            // connection has.
            Fields::File => connection.push(
                field_row(
                    "path",
                    "Database file",
                    &form.path,
                    mono_field(&form.path),
                    t,
                )
                .child(browse_button("browse-path", &form.path, t, cx))
                .into_any_element(),
            ),
            // No encryption row: the transport is HTTPS and always verified,
            // so there is no choice to show. No password either; the key
            // file is the credential.
            Fields::Account => {
                for (id, label, input, monospace) in [
                    ("account", "Account", &form.account, false),
                    ("user", "User", &form.user, false),
                    ("private-key", "Private key file", &form.private_key, true),
                    ("database", "Database", &form.database, false),
                    ("warehouse", "Warehouse", &form.warehouse, false),
                    ("role", "Role", &form.role, false),
                    // Last, because it is nearly always blank: the account
                    // names its own host.
                    ("host", "Host", &form.host, false),
                ] {
                    let control = if monospace {
                        mono_field(input)
                    } else {
                        bare(input)
                    };
                    connection.push(
                        field_row(id, label, input, control, t)
                            .when(id == "private-key", |row| {
                                row.child(browse_button("browse-private-key", input, t, cx))
                            })
                            .into_any_element(),
                    );
                }
            }
            Fields::Server => {
                // An SRV name takes no port: its records name each server's
                // own.
                let srv = form.srv && form.engine.resolves_srv();
                connection.push(
                    field_row("host", "Host", &form.host, bare(&form.host), t)
                        .when(!srv, |row| row.child(port_field(&form.port, t)))
                        .into_any_element(),
                );
                if form.engine.resolves_srv() {
                    connection.push(
                        ui::option_row(
                            "SRV host",
                            Some("The host is a DNS name whose records list the servers (mongodb+srv).".into()),
                            ui::switch("srv-host", form.srv, t).on_click(cx.listener(
                                |workspace, _, window, cx| {
                                    if let Some(form) = &mut workspace.form {
                                        form.srv = !form.srv;
                                        // The port going takes the focus with it.
                                        if form.srv && form.port.focus_handle(cx).is_focused(window) {
                                            form.needs_focus = Some(form.host.clone());
                                        }
                                        form.test = None;
                                        cx.notify();
                                    }
                                },
                            )),
                            t,
                        )
                        .into_any_element(),
                    );
                }
                let from_command = form.typed_password_command(cx).is_some();
                let password_input = if from_command {
                    bare(&form.password).disabled(true)
                } else {
                    bare(&form.password).mask_toggle()
                };
                connection.extend(
                    [
                        field_row(
                            "database",
                            "Database",
                            &form.database,
                            bare(&form.database),
                            t,
                        ),
                        field_row("user", "User", &form.user, bare(&form.user), t),
                        field_row("password", "Password", &form.password, password_input, t),
                    ]
                    .map(IntoElement::into_any_element),
                );
                // Shown for every engine that takes one rather than only once
                // typed, so no row leaves the screen with the focus in it.
                if form.target().takes_password_command() {
                    connection.push(
                        field_row(
                            "password-command",
                            "Password command",
                            &form.password_command,
                            mono_field(&form.password_command),
                            t,
                        )
                        .into_any_element(),
                    );
                    connection.push(
                        ui::option_row(
                            "Expires after",
                            Some(
                                "How long the command's password is reused for. 0 runs it \
                                 at every login."
                                    .into(),
                            ),
                            seconds_field(&form.password_expires, t),
                            t,
                        )
                        .into_any_element(),
                    );
                }
                if form.engine.takes_service() {
                    connection.push(
                        field_row("service", "Service", &form.service, bare(&form.service), t)
                            .into_any_element(),
                    );
                }
                if form.engine.takes_options() {
                    connection.push(
                        field_row(
                            "options",
                            "Options",
                            &form.options,
                            mono_field(&form.options),
                            t,
                        )
                        .into_any_element(),
                    );
                }
            }
            // The address is the driver's, inside the string, so there is no
            // host or database of DBDelve's to draw, and no URL to fill them.
            Fields::ConnectionString => {
                connection.push(
                    field_row(
                        "connection",
                        "Connection",
                        &form.connection,
                        mono_field(&form.connection),
                        t,
                    )
                    .into_any_element(),
                );
                // Under the string it reads, since an edit there is what
                // changes it; the switch is further down.
                if let Some(line) = form.tunnel_line(cx) {
                    let (text, color) = match line {
                        Ok(text) => (text, t.text_muted),
                        Err(text) => (text, t.danger),
                    };
                    connection.push(
                        div()
                            .px(px(layout::SPACE_LG))
                            .py(px(layout::SPACE_SM))
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(color)
                            .child(text)
                            .into_any_element(),
                    );
                }
                connection.extend(
                    [
                        field_row("user", "User", &form.user, bare(&form.user), t),
                        field_row(
                            "password",
                            "Password",
                            &form.password,
                            bare(&form.password).mask_toggle(),
                            t,
                        ),
                    ]
                    .map(IntoElement::into_any_element),
                );
                connection.push(
                    ui::option_row(
                        "SQL dialect",
                        Some(
                            "How DBDelve writes SQL for this connection: quoting, paging and the \
                             Read-only hold."
                                .into(),
                        ),
                        self.dialect_dropdown(cx),
                        t,
                    )
                    .into_any_element(),
                );
                connection.extend(self.dialect_suggestion(t, cx));
            }
        }

        let mut security = Vec::new();
        if fields == Fields::Server {
            security.push(
                ui::option_row(
                    "Encryption",
                    // Five words do not say which ones check who answered,
                    // and that is the whole difference between them.
                    Some(form.encryption_explanation(cx).into()),
                    self.sslmode_dropdown(cx),
                    t,
                )
                .into_any_element(),
            );
            // Only where it is consulted: with no service, a certificate file
            // on `require` changes nothing, and a field that changes nothing
            // reads as though it does. Beside a service it makes `require`
            // check, and the service may be what sets a verifying mode.
            if form.shows_root_certificate(cx) {
                security.push(
                    field_row(
                        "root-certificate",
                        "Root certificate",
                        &form.root_certificate,
                        mono_field(&form.root_certificate),
                        t,
                    )
                    .child(browse_button(
                        "browse-root-certificate",
                        &form.root_certificate,
                        t,
                        cx,
                    ))
                    .into_any_element(),
                );
            }
        }
        // A connection string's TLS is the driver's, in its own keys, so it
        // has the tunnel and no Encryption row.
        if matches!(fields, Fields::Server | Fields::ConnectionString) {
            // Host and Port are the far end of the forward, which is easy to
            // read as this machine's view of the network.
            let socket = form.target().reaches_socket_over_ssh();
            let hint = if form.ssm {
                "Connect through an AWS managed instance. Database host and port are as that \
                 instance sees them. AWS credentials come from the CLI; sign in first when using SSO."
            } else if form.ssh {
                "Connect through an SSH host. Database host and port are as the SSH host sees \
                 them; localhost refers to the SSH host itself."
            } else {
                "Connect directly to the database."
            };
            let help = |text: &'static str| {
                div()
                    .px(px(layout::SPACE_LG))
                    .pb(px(layout::SPACE_MD))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(text)
            };
            let selected = if form.ssm {
                2
            } else if form.ssh {
                1
            } else {
                0
            };
            let choices = ["None", "SSH", "AWS SSM"]
                .into_iter()
                .enumerate()
                .filter(|(index, _)| *index != 2 || form.target().takes_ssm_tunnel())
                .map(|(index, label)| {
                    ui::segmented_choice(("tunnel-kind", index), label, selected == index, true, t)
                        .debug_selector(move || {
                            if index == 1 {
                                "ssh-tunnel".into()
                            } else {
                                format!("tunnel-kind-{index}")
                            }
                        })
                        .on_click(cx.listener(move |workspace, _, _, cx| {
                            if let Some(form) = &mut workspace.form {
                                form.ssh = index == 1;
                                form.ssm = index == 2;
                                form.test = None;
                                form.needs_focus = Some(match index {
                                    1 => form.ssh_host.clone(),
                                    2 => form.ssm_target.clone(),
                                    _ => {
                                        if form.target().fields() == Fields::Server {
                                            form.password.clone()
                                        } else {
                                            form.connection.clone()
                                        }
                                    }
                                });
                                cx.notify();
                            }
                        }))
                });
            security.push(
                div()
                    .flex()
                    .flex_col()
                    .child(ui::option_row(
                        "Tunnel",
                        None,
                        ui::segmented(t).children(choices),
                        t,
                    ))
                    .child(help(hint))
                    .into_any_element(),
            );
            if form.ssm {
                if form.ssm_dependencies.is_none() && !form.ssm_checking {
                    let workspace = cx.entity().downgrade();
                    cx.defer(move |cx| {
                        let _ = workspace
                            .update(cx, |workspace, cx| workspace.check_ssm_dependencies(cx));
                    });
                }
                security.push(self.render_ssm_setup(t, cx).into_any_element());
                for (index, (id, label, input)) in [
                    ("ssm-target", "Target instance", &form.ssm_target),
                    ("ssm-profile", "AWS profile", &form.ssm_profile),
                    ("ssm-region", "AWS region", &form.ssm_region),
                ]
                .into_iter()
                .enumerate()
                {
                    let row = nested_field_row(0.0, id, label, input, bare(input), t);
                    security.push(revealed(row, "ssm-reveal", index, !cx.reduce_motion()));
                }
                security.push(
                    div()
                        .flex()
                        .px(px(layout::SPACE_LG))
                        .py(px(layout::SPACE_SM))
                        .child(
                            button(
                                "ssm-advanced",
                                if form.ssm_advanced {
                                    "▾ Advanced"
                                } else {
                                    "▸ Advanced"
                                },
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .px(px(layout::SPACE_XS))
                            .debug_selector(|| "ssm-advanced".into())
                            .on_click(cx.listener(
                                |workspace, _, _, cx| {
                                    if let Some(form) = &mut workspace.form {
                                        form.ssm_advanced = !form.ssm_advanced;
                                        if !form.ssm_advanced {
                                            form.needs_focus = Some(form.ssm_target.clone());
                                        }
                                        cx.notify();
                                    }
                                },
                            )),
                        )
                        .into_any_element(),
                );
                if form.ssm_advanced {
                    for (index, (id, label, input)) in [
                        ("ssm-aws-cli", "AWS CLI path", &form.ssm_aws_cli),
                        ("ssm-plugin", "Plugin path", &form.ssm_plugin),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let found = form
                            .ssm_dependencies
                            .as_ref()
                            .map(|results| &results[index]);
                        let row = div()
                            .debug_selector(move || format!("ssm-executable-{index}"))
                            .child(
                                nested_field_row(0.0, id, label, input, bare(input), t)
                                    .min_h(px(layout::chrome(layout::CONTROL_HEIGHT)))
                                    .child(browse_button(
                                        if index == 0 {
                                            "browse-aws-cli"
                                        } else {
                                            "browse-ssm-plugin"
                                        },
                                        input,
                                        t,
                                        cx,
                                    )),
                            );
                        use crate::db::ssm::dependencies::Status;
                        let detail = match found {
                            _ if form.ssm_checking => "Detecting…".to_owned(),
                            None => "Detecting…".to_owned(),
                            Some(Status::Found(path)) => format!("Detected: {}", path.display()),
                            Some(Status::Missing) => "Not detected".to_owned(),
                            Some(Status::WrongExecutable(_)) => "Wrong executable".to_owned(),
                            Some(Status::Unavailable(_)) => "Couldn’t run executable".to_owned(),
                        };
                        let row = row.child(
                            div()
                                .debug_selector({
                                    let detail = detail.clone();
                                    move || format!("ssm-path-{index}: {detail}")
                                })
                                .px(px(layout::SPACE_LG))
                                .pb(px(layout::SPACE_SM))
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(detail),
                        );
                        security.push(row.into_any_element());
                    }
                    security.push(help("Paths are optional. Leave blank to use PATH. After installing, choose the executable here if the desktop app cannot find it.").pt(px(layout::SPACE_MD)).into_any_element());
                }
            }
            if form.ssh {
                let reveal = !cx.reduce_motion();
                let nested = |id: &'static str,
                              label: &'static str,
                              input: &Entity<InputState>,
                              control: Input| {
                    nested_field_row(layout::SPACE_MD, id, label, input, control, t)
                };
                let mut rows = vec![
                    nested("ssh-host", "SSH host", &form.ssh_host, bare(&form.ssh_host))
                        .child(port_field(&form.ssh_port, t)),
                    nested("ssh-user", "SSH user", &form.ssh_user, bare(&form.ssh_user)),
                    nested(
                        "identity-file",
                        "Identity file",
                        &form.ssh_identity_file,
                        mono_field(&form.ssh_identity_file),
                    )
                    .child(browse_button(
                        "browse-identity-file",
                        &form.ssh_identity_file,
                        t,
                        cx,
                    )),
                    nested(
                        "ssh-passphrase",
                        "Key passphrase",
                        &form.ssh_passphrase,
                        bare(&form.ssh_passphrase).mask_toggle(),
                    ),
                ];
                if socket {
                    rows.push(
                        div()
                            .child(nested(
                                "ssh-run-as",
                                "Run as",
                                &form.ssh_run_as,
                                bare(&form.ssh_run_as),
                            ))
                            .child(help(
                                "For a Unix socket host such as /var/run/postgresql, sudo opens \
                                 the socket as this OS user for peer authentication.",
                            )),
                    );
                }
                security.extend(
                    rows.into_iter()
                        .enumerate()
                        .map(|(index, row)| revealed(row, "ssh-reveal", index, reveal)),
                );
            }
        }

        let mut session = Vec::new();
        // The target decides, as it does for the rows above: a plugin's
        // process and an engine with no session to set draw none.
        if form.target().takes_time_zone() {
            session.push(
                ui::option_row(
                    "Time zone",
                    Some(
                        "Applied as the session default on every session of this \
                         connection. A name such as Europe/Paris or an offset such as \
                         +02:00; blank leaves the server's own."
                            .into(),
                    ),
                    div()
                        .flex()
                        .items_center()
                        .w(px(layout::chrome(layout::FORM_TIMEOUT_WIDTH * 2.0)))
                        .h(px(Control::Compact.height()))
                        .px(px(layout::SPACE_SM))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .border_1()
                        .border_color(t.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(gpui_component::Sizable::small(bare(&form.time_zone))),
                        ),
                    t,
                )
                .debug_selector(|| "form-time-zone".into())
                .into_any_element(),
            );
        }
        if form.target().takes_startup_script() {
            session.push(
                ui::stacked_option_row(
                    "Startup script",
                    Some(
                        "Runs on every session DBDelve opens for this connection, checked \
                         against its access mode first. Stored in profiles.toml as plain \
                         text."
                            .into(),
                    ),
                    div()
                        .w_full()
                        .child(Textarea::new(&form.startup_script).font_family(mono.clone())),
                    t,
                )
                .into_any_element(),
            );
        }

        let listed = form.target().driver_properties();
        if !listed.is_empty() {
            let keys = listed
                .iter()
                .map(|property| property.key())
                .collect::<Vec<_>>()
                .join(", ");
            session.push(
                ui::stacked_option_row(
                    "Driver properties",
                    Some(
                        format!(
                            "One key = value per line; only {keys} are taken. Applied to \
                             every session of this connection."
                        )
                        .into(),
                    ),
                    div()
                        .w_full()
                        .child(Textarea::new(&form.properties).font_family(mono.clone())),
                    t,
                )
                .debug_selector(|| "form-driver-properties".into())
                .into_any_element(),
            );
        }

        let mut behaviour = Vec::new();
        // Only while creating: past that, `Workspace::set_mode` is the one
        // door a mode changes through, from the titlebar, and it pushes the
        // change into live grids this form has no route to.
        if form.editing.is_none() {
            let access = Mode::ALL.into_iter().enumerate().map(|(index, mode)| {
                ui::segmented_choice(("access", index), mode.label(), mode == form.mode, true, t)
                    .on_click(cx.listener(move |workspace, _, _, cx| {
                        if let Some(form) = &mut workspace.form {
                            form.mode = mode;
                            // A pass without the Read-only hold did not test it.
                            form.test = None;
                            cx.notify();
                        }
                    }))
            });
            behaviour.push(
                ui::option_row("Access", None, ui::segmented(t).children(access), t)
                    .into_any_element(),
            );
        }
        // The colour only ever labels a connection, so nothing here can make
        // the form invalid and nothing has to move focus.
        let swatches = std::iter::once(None)
            .chain(ConnectionColor::ALL.map(Some))
            .enumerate()
            .map(|(index, color)| {
                ui::swatch(("color", index), color, color == form.color, t).on_click(cx.listener(
                    move |workspace, _, _, cx| {
                        if let Some(form) = &mut workspace.form {
                            form.color = color;
                            cx.notify();
                        }
                    },
                ))
            });
        behaviour.push(
            ui::option_row(
                "Color",
                None,
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .children(swatches),
                t,
            )
            .into_any_element(),
        );
        behaviour
            .push(ui::option_row("Project", None, self.project_dropdown(cx), t).into_any_element());
        // Every engine can stop a statement, and this is the only place a
        // profile has to say for how long.
        behaviour.push(
            ui::option_row(
                "Statement timeout",
                None,
                seconds_field(&form.statement_timeout, t),
                t,
            )
            .into_any_element(),
        );
        // Asked of the target, so the engines that keep a session open gain the
        // row by turning the fact on, with no change here.
        if form.target().keeps_alive() {
            behaviour.push(
                ui::option_row(
                    "Keep-alive",
                    Some(
                        "Pings the connection's open sessions this often so an idle cut-off \
                         between here and the server does not close them. Off when blank."
                            .into(),
                    ),
                    seconds_field(&form.keep_alive, t),
                    t,
                )
                .into_any_element(),
            );
        }
        // Every engine's connection can be left unused, so every target draws it.
        behaviour.push(
            ui::option_row(
                "Idle timeout",
                Some(
                    "Closes this connection's sessions after this long without use, never \
                     while work is unfinished. Off when blank."
                        .into(),
                ),
                number_field(&form.idle_timeout, "min", t),
                t,
            )
            .into_any_element(),
        );
        // Every target's connect runs the loads this chooses among.
        let depths = Depth::ALL.into_iter().enumerate().map(|(index, depth)| {
            ui::segmented_choice(
                ("introspection", index),
                depth.label(),
                depth == form.depth,
                true,
                t,
            )
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    form.depth = depth;
                    cx.notify();
                }
            }))
        });
        behaviour.push(
            ui::option_row(
                "Introspection",
                Some(
                    "What a connect reads before the tree is drawn: relations always, then \
                     routines and sequences, then table sizes while Show sizes is on."
                        .into(),
                ),
                ui::segmented(t).children(depths),
                t,
            )
            .into_any_element(),
        );

        let section = |label: &'static str, rows: Vec<AnyElement>| {
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .px(px(layout::SPACE_XS))
                        .child(section_label(t, label)),
                )
                .child(ui::well(t, rows))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(layout::FORM_SECTION_GAP))
            .px(px(layout::FORM_PADDING))
            .pb(px(layout::FORM_PADDING))
            .child(section("Connection", connection))
            .when(!security.is_empty(), |body| {
                body.child(section("Security", security))
            })
            .when(!session.is_empty(), |body| {
                body.child(section("Session", session))
            })
            .child(section("Behaviour", behaviour))
    }

    /// Test and what it last said, then the way out and the way in.
    fn render_form_footer(&self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        let status = footer_status(
            form.notice.as_ref(),
            form.error.as_ref(),
            form.test.as_ref(),
        );
        let commit = if form.editing.is_some() {
            "Save"
        } else {
            "Connect"
        };
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .px(px(layout::FORM_PADDING))
            .py(px(layout::SPACE_MD))
            .border_t_1()
            .border_color(t.border)
            .child(
                button("test-connection", "Test", Tone::Quiet, Control::Standard, t)
                    .border_1()
                    .border_color(t.border)
                    .on_click(cx.listener(Self::test_connection)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .children(status.map(|(status, text)| {
                        div()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_SM))
                            .child(ui::status_dot(t, status))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .text_color(if status == Status::Failed {
                                        t.danger
                                    } else {
                                        t.text_muted
                                    })
                                    .child(text),
                            )
                    })),
            )
            .child(
                button("cancel", "Cancel", Tone::Quiet, Control::Standard, t)
                    .on_click(cx.listener(|workspace, _, _, cx| workspace.close_form(cx))),
            )
            .child(
                ui::accent_button("connect", commit, Control::Standard, t, cx)
                    .children(
                        ui::chord_for("submit_connection", &self.settings.custom_keybindings)
                            .map(|chord| ui::keycaps_on_accent(&chord, t)),
                    )
                    .on_click(cx.listener(Self::connect)),
            )
    }

    fn sslmode_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        Self::form_dropdown(
            "sslmode",
            form.sslmode,
            vec![SslMode::ALL.to_vec()],
            |mode| mode.label().into(),
            |_| None,
            |form, mode| form.set_sslmode(mode),
            cx,
        )
    }

    fn dialect_dropdown(&self, cx: &mut Context<Self>) -> AnyElement {
        let form = self.form.as_ref().expect("drawn only on the form");
        Self::form_dropdown(
            "dialect",
            form.engine,
            vec![dialects()],
            |engine| engine.label().into(),
            |_| None,
            |form, engine| form.set_dialect(engine),
            cx,
        )
    }

    /// The dialect the driver's last Test said the server speaks, when it is
    /// not the one picked: one click to take it, never taken without one.
    fn dialect_suggestion(&self, t: Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.form.as_ref()?;
        let Some(ConnectionTest::Passed(Some(reported))) = &form.test else {
            return None;
        };
        let engine = suggested_dialect(form.engine, reported)?;
        // Only the overlap with a built-in is worth a sentence: the built-in
        // does what a plugin speaking the same dialect cannot.
        let through_plugin = Target {
            engine,
            ..form.target()
        };
        let built_in = engine.has_built_in_driver().then(|| {
            let more = match through_plugin.edits_rows() {
                true => "imports rows and copies DDL",
                false => "can edit rows",
            };
            format!(
                "DBDelve also has a built-in {} driver, which {more}.",
                engine.label()
            )
            .into()
        });
        let take = engine.speaks_through_plugins().then(|| {
            button(
                "use-dialect",
                format!("Use {}'s SQL", engine.label()),
                Tone::Quiet,
                Control::Standard,
                t,
            )
            .border_1()
            .border_color(t.border)
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(form) = &mut workspace.form {
                    form.set_dialect(engine);
                    // The chip unmounts once the dialect matches; a keyboard
                    // activation would leave focus on it.
                    form.needs_focus = Some(form.connection.clone());
                }
                cx.notify();
            }))
        });
        Some(
            ui::option_row(
                format!("The driver reports {}", reported.dbms),
                built_in,
                div().children(take),
                t,
            )
            .into_any_element(),
        )
    }

    fn check_ssm_dependencies(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        if !form.ssm || form.ssm_checking {
            return;
        }
        form.ssm_checking = true;
        let input = form.ssm_aws_cli.clone();
        let paths = [
            form.ssm_aws_cli.read(cx).value().trim().to_owned(),
            form.ssm_plugin.read(cx).value().trim().to_owned(),
        ];
        let checked = paths.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |workspace, cx| {
            // Coalesce edits without starting a process for every keystroke.
            executor.timer(Duration::from_millis(400)).await;
            let current = workspace
                .update(cx, |workspace, cx| {
                    let Some(form) = &mut workspace.form else {
                        return false;
                    };
                    if form.ssm_aws_cli != input {
                        return false;
                    }
                    if !form.ssm
                        || form.ssm_aws_cli.read(cx).value().trim() != paths[0]
                        || form.ssm_plugin.read(cx).value().trim() != paths[1]
                    {
                        form.ssm_checking = false;
                        cx.notify();
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let results = spawn_blocking(&executor, move || {
                crate::db::ssm::dependencies::check(
                    (!checked[0].is_empty()).then(|| std::path::Path::new(&checked[0])),
                    (!checked[1].is_empty()).then(|| std::path::Path::new(&checked[1])),
                )
            })
            .await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(form) = &mut workspace.form else {
                    return;
                };
                if form.ssm_aws_cli != input {
                    return;
                }
                form.ssm_checking = false;
                if form.ssm_aws_cli.read(cx).value().trim() == paths[0]
                    && form.ssm_plugin.read(cx).value().trim() == paths[1]
                {
                    form.ssm_dependencies = Some(results);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_ssm_setup(&self, t: Theme, cx: &mut Context<Self>) -> Div {
        use crate::db::ssm::dependencies::Status as ToolStatus;
        let form = self.form.as_ref().expect("form is open");
        let mut setup = div()
            .px(px(layout::SPACE_LG))
            .py(px(layout::SPACE_SM))
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_SM))
            .text_size(px(layout::chrome(layout::TEXT_SM)));
        for (index, (label, url)) in [
            ("AWS CLI", "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html"),
            ("Session Manager plugin", "https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html"),
        ].into_iter().enumerate() {
            let status = form.ssm_dependencies.as_ref().map(|results| &results[index]);
            let title = match status {
                _ if form.ssm_checking => "Checking…",
                Some(ToolStatus::Found(_)) => "Found",
                Some(ToolStatus::Missing) => "Missing",
                Some(ToolStatus::WrongExecutable(_)) => "Wrong executable",
                Some(ToolStatus::Unavailable(_)) => "Couldn’t run",
                None => "Checking…",
            };
            let mut row = div().flex().flex_col().gap(px(layout::SPACE_XS))
                .child(div().flex().items_center().justify_between().gap(px(layout::SPACE_MD))
                    .child(div().text_color(t.text).child(label))
                    .child(div().debug_selector(move || format!("ssm-status-{index}: {title}")).text_color(t.text_muted).child(title)));
            if let Some(ToolStatus::Unavailable(message) | ToolStatus::WrongExecutable(message)) = status {
                row = row.child(div().text_color(t.text_muted).child(message.clone()));
            }
            if matches!(status, Some(ToolStatus::Missing | ToolStatus::Unavailable(_) | ToolStatus::WrongExecutable(_))) {
                row = row.child(div().flex().flex_wrap().items_center().gap(px(layout::SPACE_SM))
                    .child(div().rounded(px(layout::RADIUS_CONTROL)).border_1().border_color(t.border).bg(t.element_active)
                        .child(button(("ssm-choose", index), "Choose executable…", Tone::Primary, Control::Compact, t)
                        .debug_selector(move || format!("ssm-choose-{index}"))
                        .on_click(cx.listener(move |workspace, _, window, cx| {
                            if let Some(form) = &mut workspace.form {
                                form.ssm_advanced = true;
                                let input = if index == 0 { form.ssm_aws_cli.clone() } else { form.ssm_plugin.clone() };
                                cx.notify();
                                workspace.browse_into(input, window, cx);
                            }
                        }))))
                    .child(button(("ssm-install", index), "Installation guide ↗", Tone::Quiet, Control::Compact, t)
                        .debug_selector(move || format!("ssm-install-{index}"))
                        .on_click(move |_, _, cx| cx.open_url(url))));
            }
            setup = setup.child(row);
        }
        setup
            .child(
                div().text_color(t.text_muted).child(
                    "Checks installed tools only. Use Test to verify AWS and database access.",
                ),
            )
            .child(
                div().flex().child(
                    div()
                        .rounded(px(layout::RADIUS_CONTROL))
                        .border_1()
                        .border_color(t.border)
                        .bg(t.element_active)
                        .child(
                            button(
                                "ssm-check-again",
                                "Check again",
                                Tone::Primary,
                                Control::Compact,
                                t,
                            )
                            .debug_selector(|| "ssm-check-again".into())
                            .disabled(form.ssm_checking)
                            .on_click(cx.listener(
                                |workspace, _, _, cx| {
                                    workspace.check_ssm_dependencies(cx);
                                },
                            )),
                        ),
                ),
            )
    }

    /// The native Open dialog, for a file found rather than typed into
    /// `input`. A dismissed dialog leaves the field as it was.
    fn browse_into(
        &mut self,
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let chosen = chosen.await;
            _ = workspace.update_in(cx, |workspace, window, cx| match chosen {
                Ok(Ok(Some(paths))) => {
                    let (Some(path), Some(form)) = (paths.into_iter().next(), &mut workspace.form)
                    else {
                        return;
                    };
                    // The dialog does not hold the window: by now the form may
                    // be another one, or its field gone with the engine or a
                    // switch, and the pick no longer has anywhere to go.
                    if !shown_file_fields(form, cx).contains(&&input) {
                        return;
                    }
                    let path = path.to_string_lossy().into_owned();
                    if input == form.ssm_aws_cli || input == form.ssm_plugin {
                        form.ssm_dependencies = None;
                    }
                    input.update(cx, |input, cx| input.set_value(path, window, cx));
                    form.needs_focus = Some(input);
                    // A pass was for the file that was there before.
                    form.test = None;
                    cx.notify();
                }
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    /// Closes the form without saving, from its × or its Cancel.
    pub(crate) fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        self.refocus_front();
        cx.notify();
    }

    /// Where an import's connections go, asked before any is added: a new
    /// project named for the client by default, one already made, or none.
    /// The dialogs, under a focus of their own that the one going up takes
    /// from whatever held the keyboard: left in the editor or the grid behind
    /// it, `enter` went on editing what the dialog covers.
    pub(crate) fn dialog_layer(
        &self,
        dialogs: impl IntoIterator<Item = (&'static str, Option<AnyElement>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (names, dialogs): (Vec<_>, Vec<_>) = dialogs
            .into_iter()
            .filter_map(|(name, dialog)| Some((name, dialog?)))
            .unzip();
        let front = names.last().copied();
        let behind = self.dialog_in_front.replace(front);
        let raised = behind != front;
        if dialogs.is_empty() {
            // The last one closing unmounts the keyboard with it. Handed back
            // only after the frame, and only if nothing took it during it:
            // Edit anyway puts it in the cell input the grid opens as it draws.
            if self.dialog_focus.contains_focused(window, cx) {
                let stranded = window.focused(cx);
                cx.on_next_frame(window, move |workspace, window, cx| {
                    let before = workspace.focus_before_dialog.take();
                    if window.focused(cx) != stranded {
                        return;
                    }
                    // Back where it was taken from while that is still drawn:
                    // a cell input the mode prompt went up over waits there
                    // for the `enter` that commits it.
                    match (before, workspace.form.as_mut()) {
                        (Some(before), _) if workspace.focus.contains(&before, window) => {
                            before.focus(window, cx)
                        }
                        (_, Some(form)) => form.needs_focus = Some(form.lead_field()),
                        (_, None) => workspace.refocus_front(),
                    }
                    cx.notify();
                });
            }
            return None;
        }
        if behind.is_none() && !self.dialog_focus.contains_focused(window, cx) {
            self.focus_before_dialog.replace(window.focused(cx));
        }
        // The palette and MCP popover draw over dialogs without changing which
        // one is in front, so an approval under either is not being shown.
        let covered = self.palette.is_some() || self.agent_popover_open;
        // An approval's Approve counts its wait from when the person can see the
        // dialog: raised to the front, a different call come to the front, or
        // the palette over it gone.
        if front == Some("agent-approval")
            && let Some(id) = self.front_approval_id()
        {
            if covered {
                // Forgotten, so it is revealed afresh once the palette goes;
                // the window still asks for attention once, as it may be
                // behind another application.
                if self.agent.revealed.is_some() || self.agent.attended != Some(id) {
                    cx.on_next_frame(window, move |workspace, window, cx| {
                        if workspace.agent.attended != Some(id) {
                            workspace.reveal_approval(window, cx);
                        }
                        workspace.agent.revealed = None;
                    });
                }
            } else if raised || self.agent.revealed != Some(id) {
                cx.on_next_frame(window, |workspace, window, cx| {
                    workspace.reveal_approval(window, cx)
                });
            }
        }
        // An Approve is live only while its own dialog is the one in front and
        // nothing covers it: a dialog or the palette raised over it, or another
        // call in front of it, ends the arm, so what is shown again is armed
        // afresh.
        if self.agent.armed.is_some()
            && (covered
                || front != Some("agent-approval")
                || self.front_approval_id() != self.agent.armed)
        {
            cx.on_next_frame(window, |workspace, _, cx| {
                if workspace.palette.is_some()
                    || workspace.agent_popover_open
                    || workspace.dialog_in_front.get() != Some("agent-approval")
                    || workspace.front_approval_id() != workspace.agent.armed
                {
                    workspace.agent.armed = None;
                    cx.notify();
                }
            });
        }
        if raised {
            // Every dialog opens on its primary button, as a native one does,
            // except the one that ends the user's transaction and the one that
            // widens what an agent may reach: they open on Cancel, so an `enter`
            // held or repeated from the click that raised them cancels rather
            // than commits, discards or grants. The ring is drawn by the dialogs
            // above, before this ran.
            let first = match front {
                // Cancel, wherever a failed transaction's missing Commit puts it.
                Some("end-transaction") => self.dialog_buttons().saturating_sub(1),
                _ => first_choice(front),
            };
            self.dialog_choice.set(first);
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        // The palette is stacked over the dialogs and holds the keyboard
        // itself. A field inside the dialog in front keeps it, but not one in
        // a dialog another has just gone up over, or come down off with the
        // focused button in it.
        if self.palette.is_none() && (raised || !self.dialog_focus.contains_focused(window, cx)) {
            self.dialog_focus.focus(window, cx);
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .track_focus(&self.dialog_focus)
                .key_context("Dialog")
                .on_action(cx.listener(Self::confirm_dialog))
                .on_action(cx.listener(|workspace, _: &TickDialogBox, window, cx| {
                    let ringed = workspace.dialog_checkbox()
                        && workspace.dialog_choice.get() + 1 == workspace.dialog_buttons();
                    match ringed {
                        true => workspace.confirm_dialog(&ConfirmDialog, window, cx),
                        false => cx.propagate(),
                    }
                }))
                .on_action(
                    cx.listener(|workspace, _: &PreviousDialogButton, window, cx| {
                        workspace.step_dialog_button(true, window, cx);
                    }),
                )
                .on_action(cx.listener(|workspace, _: &NextDialogButton, window, cx| {
                    workspace.step_dialog_button(false, window, cx);
                }))
                .children(dialogs)
                .into_any_element(),
        )
    }

    /// A dialog's button in the ring that marks the one `enter` presses,
    /// counted from the right so the primary is 0. Every button wears the
    /// frame, pulled out by its own width, so moving the ring moves nothing.
    pub(crate) fn choice(
        &self,
        dialog: &'static str,
        from_right: usize,
        button: impl IntoElement,
        t: Theme,
    ) -> Div {
        let chosen =
            self.dialog_in_front.get() == Some(dialog) && self.dialog_choice.get() == from_right;
        div()
            .debug_selector(|| format!("{dialog}-{from_right}"))
            .m(px(-2.))
            .p(px(1.))
            .border_1()
            .rounded(px(layout::RADIUS_CONTROL + 2.))
            .border_color(match chosen {
                true => t.accent.into(),
                false => gpui::transparent_black(),
            })
            .child(button)
    }

    /// How many controls the dialog in front has to move the ring across:
    /// its buttons, then its "Don't ask again" box or its project, which sit
    /// left of and above them. None for the import and the schema export,
    /// forms with controls of their own, which Tab moves the keyboard itself
    /// through.
    fn dialog_buttons(&self) -> usize {
        let checkbox = usize::from(self.dialog_checkbox());
        match self.dialog_in_front.get() {
            Some("stale-edit") => {
                let editing = self
                    .profile()
                    .and_then(|profile| profile.session.stale_edit.as_ref())
                    .is_some_and(|stale| stale.resume.is_some());
                if editing { 3 + checkbox } else { 2 }
            }
            Some("import-choice") => 3,
            Some("pending-export") => {
                let all = self
                    .profile()
                    .and_then(|profile| profile.session.pending_export.as_ref())
                    .is_some_and(|pending| pending.every_row.is_ok());
                if all { 3 } else { 2 }
            }
            Some("stable-import") => self.stable_import_buttons(),
            Some("unsaved") => 3,
            // Forms with fields and a list of their own, which Tab moves the
            // keyboard through and whose buttons act through their own handlers.
            Some("import-rows" | "schema-export" | "filter-picker" | "role-form") | None => 0,
            Some("show-grants") => 1,
            // Deny alone: Approve is click-only and never a stop.
            Some("agent-approval") => 1,
            Some("agent-audit") => 1,
            // Commit is not offered over a failed transaction.
            Some("end-transaction") => match self
                .profile()
                .is_some_and(crate::session::Profile::transaction_failed)
            {
                true => 2,
                false => 3,
            },
            Some(_) => 2 + checkbox,
        }
    }

    /// Whether the dialog in front offers to stop asking: the run prompt over
    /// a kind that can be silenced or a statement dbdelve cannot parse, and the
    /// stale-edit prompt over an edit.
    pub(crate) fn dialog_checkbox(&self) -> bool {
        let Some(profile) = self.profile() else {
            return false;
        };
        match self.dialog_in_front.get() {
            Some("pending-run") => profile.session.pending_run.as_ref().is_some_and(|pending| {
                match sql::gate(&pending.verdict, profile.mode, &profile.confirmed) {
                    Some(Stop::Confirm(kind)) => kind.suppressible(),
                    Some(Stop::RunOnce) => pending.own_run,
                    _ => false,
                }
            }),
            Some("stale-edit") => profile
                .session
                .stale_edit
                .as_ref()
                .is_some_and(|stale| stale.resume.is_some()),
            _ => false,
        }
    }

    fn step_dialog_button(&mut self, leftward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let buttons = self.dialog_buttons();
        if buttons == 0 {
            self.step_dialog_focus(leftward, window, cx);
            return;
        }
        // The ring is where the keyboard is, so the layer takes it back from
        // anything inside that took it, or `enter` would go there instead.
        if !self.dialog_focus.is_focused(window) {
            self.dialog_focus.focus(window, cx);
        }
        let choice = self.dialog_choice.get();
        self.dialog_choice.set(match leftward {
            true => (choice + 1) % buttons,
            false => (choice + buttons - 1) % buttons,
        });
        cx.notify();
    }

    /// Tab order through the import's controls, kept inside the dialog. Its
    /// preview grid and its SQL are passed over: each binds `tab` itself, and
    /// once in either the keyboard could never Tab back out.
    fn step_dialog_focus(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let passed_over = self
            .profile()
            .and_then(|profile| profile.session.import_rows.as_ref())
            .map(|dialog| vec![dialog.preview.focus_handle(cx), dialog.sql.focus_handle(cx)])
            .unwrap_or_default();
        let mut first = None;
        loop {
            match backward {
                true => window.focus_prev(cx),
                false => window.focus_next(cx),
            }
            let focused = window.focused(cx);
            let landed = self.dialog_focus.contains_focused(window, cx)
                && !passed_over
                    .iter()
                    .any(|handle| handle.contains_focused(window, cx));
            if landed || focused.is_none() {
                break;
            }
            match &first {
                None => first = focused,
                Some(first) if Some(first) == focused.as_ref() => {
                    self.dialog_focus.focus(window, cx);
                    break;
                }
                Some(_) => {}
            }
        }
        cx.notify();
    }

    /// `enter` presses the button the ring is on, in the dialog drawn in
    /// front, and ticks or unticks its box -- wherever focus is inside it.
    /// The import has no ring: a control there that holds focus itself takes
    /// the key as it would anywhere.
    pub(crate) fn confirm_dialog(
        &mut self,
        _: &ConfirmDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_buttons() == 0 && !self.dialog_focus.is_focused(window) {
            cx.propagate();
            return;
        }
        let choice = self.dialog_choice.get();
        let Some(dialog) = self.dialog_in_front.get() else {
            return;
        };
        let session = self.profile().map(|profile| &profile.session);
        let editing = session
            .and_then(|session| session.stale_edit.as_ref())
            .is_some_and(|stale| stale.resume.is_some());
        let deleting = session.and_then(|session| session.pending_delete.clone());
        let running = session.is_some_and(|session| {
            matches!(session.active_query(), Some(QueryState::Running { .. }))
        });
        let every_row = session
            .and_then(|session| session.pending_export.as_ref())
            .is_some_and(|pending| pending.every_row.is_ok());
        match (dialog, choice) {
            ("import-choice", 0) => self.confirm_import(window, cx),
            ("import-choice", 2) => self.open_palette(PaletteMode::ImportProject, window, cx),
            ("import-choice", _) => {
                self.pending_import = None;
                cx.notify();
            }
            ("stable-import", choice) => self.answer_stable_import(choice, cx),
            ("stale-edit", 3) if editing => self.toggle_stale_dont_ask(cx),
            ("stale-edit", 0) if editing => self.edit_stale_anyway(window, cx),
            ("stale-edit", 0) => self.refresh_stale(window, cx),
            ("stale-edit", 1) if editing => self.refresh_stale(window, cx),
            ("stale-edit", _) => _ = self.cancel_stale_edit(cx),
            ("queue-failure", 0) => self.continue_queue(cx),
            ("queue-failure", _) => self.stop_queue(cx),
            ("pending-run", 2) => self.toggle_dont_ask(cx),
            ("pending-export", 0) => self.answer_pending_export(every_row, cx),
            ("pending-export", 1) if every_row => self.answer_pending_export(false, cx),
            ("pending-export", _) => _ = self.cancel_pending_export(cx),
            ("pending-run", 0) => self.approve_pending_run(cx),
            ("pending-run", _) => self.cancel_pending_run(cx),
            ("agent-visibility", 0) => self.confirm_agent_visibility(cx),
            ("agent-visibility", _) => _ = self.cancel_agent_visibility(cx),
            ("agent-writes-auto", 0) => self.confirm_agent_writes_auto(cx),
            ("agent-writes-auto", _) => _ = self.cancel_agent_writes_auto(cx),
            // Every index denies: no key approves an agent's call.
            ("agent-approval", _) => _ = self.deny_front_approval(cx),
            ("agent-audit", _) => _ = self.close_agent_audit(cx),
            ("discard", 0) => self.confirm_discard_close(window, cx),
            ("discard", _) => _ = self.cancel_discard_close(cx),
            ("quit", 0) => self.confirm_quit(cx),
            ("quit", _) => _ = self.cancel_quit(cx),
            ("forget-file", 0) => self.confirm_forget_file(cx),
            ("forget-file", _) => _ = self.cancel_forget_file(cx),
            ("end-transaction", choice) => match self.end_transaction_choice(choice) {
                Some(verb) => self.end_transaction_for_auto(verb, window, cx),
                None => _ = self.cancel_auto_commit_prompt(cx),
            },
            ("delete", 0) => match deleting {
                Some(PendingDelete::Query(name)) => self.delete_saved_query(name, window, cx),
                Some(PendingDelete::Folder(name, _)) => self.delete_folder(name, cx),
                None => {}
            },
            ("delete", _) => _ = self.cancel_delete_query(cx),
            // The button is disabled while the batch runs.
            ("apply-review", 0) if !running => self.run_apply_review(cx),
            ("apply-review", 1) => _ = self.close_apply_review(cx),
            ("show-ddl", 0) => _ = self.close_ddl_view(cx),
            ("show-grants", _) => _ = self.close_grants_view(cx),
            ("role-form", 0) => self.submit_role_form(window, cx),
            ("role-form", _) => _ = self.close_role_form(cx),
            ("show-ddl", _) => self.copy_shown_ddl(cx),
            _ => {}
        }
    }

    /// Where an import's connections can go, a group per separator: a new
    /// project named for the client unless one already is, the projects
    /// there are, and none.
    pub(crate) fn import_destinations(&self) -> Option<Vec<Vec<Option<String>>>> {
        let pending = self.pending_import.as_ref()?;
        let known = self.projects.iter().map(|project| project.name.clone());
        let suggested = format!("Imported from {}", pending.source.label());
        let mut groups = Vec::new();
        if !self
            .projects
            .iter()
            .any(|project| project.name == suggested)
        {
            groups.push(vec![Some(suggested)]);
        }
        if !self.projects.is_empty() {
            groups.push(known.map(Some).collect());
        }
        groups.push(vec![None]);
        Some(groups)
    }

    pub(crate) fn set_import_project(&mut self, project: Option<String>) {
        if let Some(pending) = &mut self.pending_import {
            pending.project = project;
        }
    }

    /// Whether picking `project` for the import makes it.
    pub(crate) fn import_makes(&self, project: &Option<String>) -> bool {
        project
            .as_ref()
            .is_some_and(|name| !self.projects.iter().any(|known| &known.name == name))
    }

    pub(crate) fn render_import_choice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let pending = self.pending_import.as_ref()?;
        let source = pending.source.label();
        let groups = self.import_destinations()?;
        let new: Vec<Option<String>> = groups
            .iter()
            .flatten()
            .filter(|project| self.import_makes(project))
            .cloned()
            .collect();
        let marked = new.clone();
        let picker = Self::dropdown(
            "import-project",
            pending.project.clone(),
            groups,
            move |project| match project {
                None => "No project".into(),
                Some(name) if new.contains(project) => format!("{name} (new project)").into(),
                Some(name) => name.clone().into(),
            },
            move |project| {
                let path = match project {
                    None => return None,
                    Some(_) if marked.contains(project) => icon::PLUS,
                    Some(_) => icon::PROJECT,
                };
                Some(row_icon(t, path).into_any_element())
            },
            |workspace, project, _, _| workspace.set_import_project(project),
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
                        .child(self.choice(
                            "import-choice",
                            2,
                            self.labelled_field("Project", picker, cx),
                            t,
                        ))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "import-choice",
                                        1,
                                        button(
                                            "cancel-import",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            cx.listener(|workspace, _, _, cx| {
                                                workspace.pending_import = None;
                                                cx.notify();
                                            }),
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "import-choice",
                                        0,
                                        button(
                                            "confirm-import",
                                            "Import",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            cx.listener(|workspace, _, window, cx| {
                                                workspace.confirm_import(window, cx);
                                            }),
                                        ),
                                        t,
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
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
        let at = popup.at?;
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
                            if let Some(popup) = workspace.reference_popup.take() {
                                workspace.open_listed_reference(&popup.source, index, window, cx);
                            }
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
                        .left(at.x)
                        .top(at.y)
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
        let pending = self.profile()?.session.pending_discard.as_ref()?;
        if pending.unsaved {
            return None;
        }
        // Close All Tabs holds the unsaved files with them, for the question
        // after this one.
        let held = 1 + pending
            .also
            .iter()
            .filter(|tab| self.would_lose_on_close(**tab, cx))
            .count();
        let (title, sentence) = match pending.row {
            _ if held > 1 => (
                "Close tabs",
                format!(
                    "{held} tabs have cell edits that have not been applied. \
                     Closing them discards them."
                ),
            ),
            Some(_) => pending.then.discards_refused_row(),
            None => pending.then.discards(),
        };
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
                        .child(section_label(t, title))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                // The edits are held against the fetched rows
                                // and never written to them, so whatever
                                // replaces or closes them is the moment they
                                // stop existing.
                                .child(sentence),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "discard",
                                        1,
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
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "discard",
                                        0,
                                        button(
                                            "confirm-discard-close",
                                            "Discard",
                                            Tone::Danger,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                _ = discard_workspace.update(
                                                    cx,
                                                    |workspace, cx| {
                                                        workspace.confirm_discard_close(window, cx);
                                                    },
                                                );
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

    /// Remove / Keep, over a recent file found gone when its row was opened.
    pub(crate) fn render_forget_file(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.profile()?.session.pending_forget.as_ref()?;
        Some(self.question(
            "forget-file",
            "Open",
            format!(
                "{} is not there any more. Remove it from the list?",
                crate::sql_file::file_name(path)
            ),
            [
                ("keep-file", "Keep", Tone::Quiet, |workspace, _, cx| {
                    workspace.cancel_forget_file(cx);
                }),
                (
                    "forget-file",
                    "Remove",
                    Tone::Primary,
                    |workspace, _, cx| workspace.confirm_forget_file(cx),
                ),
            ],
            cx,
        ))
    }

    /// A dialog asking one line, its buttons left to right, the last the
    /// primary the ring opens on.
    fn question<const N: usize>(
        &self,
        name: &'static str,
        heading: &str,
        question: String,
        buttons: [(&'static str, &'static str, Tone, DialogAnswer); N],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = popup_theme(cx);
        let buttons = buttons
            .into_iter()
            .enumerate()
            .map(|(at, (id, label, tone, answer))| {
                let workspace = cx.entity().downgrade();
                self.choice(
                    name,
                    N - 1 - at,
                    button(id, label, tone, Control::Standard, t).on_click(move |_, window, cx| {
                        _ = workspace.update(cx, |workspace, cx| answer(workspace, window, cx));
                    }),
                    t,
                )
            });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(
                dialog(t)
                    .child(section_label(t, heading))
                    .child(
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text_muted)
                            .child(question),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(layout::SPACE_SM))
                            .children(buttons),
                    ),
            )
            .into_any_element()
    }

    /// An object's definition as the server wrote it, read-only: a header and a
    /// block that scrolls in the editor's face, drawn like the apply review's
    /// statement but with nothing to run, so there is no editor and no buffer
    /// behind it. Copy is the only way the text leaves.
    pub(crate) fn render_show_ddl(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.ddl_view_in_front()?;
        let t = popup_theme(cx);
        let code = fonts(cx).editor.clone();
        let syntax = self.engine().syntax();
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let copyable = view
            .sections
            .iter()
            .any(|section| matches!(&section.text, DdlText::Ready(text) if !text.is_empty()));
        let highlight = cx.theme().highlight_theme.clone();
        // One definition as the dialog draws it. `suffix` tells the sections
        // of several apart; `scrolls` is whether this block scrolls itself,
        // which several leave to the list that holds them.
        let block = |text: &DdlText, name: &str, suffix: String, scrolls: bool| -> AnyElement {
            match text {
                DdlText::Loading => div()
                    .debug_selector({
                        let suffix = suffix.clone();
                        move || format!("show-ddl-loading{suffix}")
                    })
                    .text_color(t.text_muted)
                    .child("Reading the definition…")
                    .into_any_element(),
                DdlText::Failed(message) => div()
                    .debug_selector({
                        let suffix = suffix.clone();
                        move || format!("show-ddl-failed{suffix}")
                    })
                    .text_color(t.danger)
                    .child(message.clone())
                    .into_any_element(),
                DdlText::Ready(text) if text.is_empty() => div()
                    .debug_selector({
                        let suffix = suffix.clone();
                        move || format!("show-ddl-empty{suffix}")
                    })
                    .text_color(t.text_muted)
                    .child(format!("The server returned no definition for {name}."))
                    .into_any_element(),
                // Nothing moves in it, so it takes no smooth-scroll modifier.
                DdlText::Ready(text) => div()
                    .id(format!("show-ddl-text{suffix}"))
                    .debug_selector(move || format!("show-ddl-text{suffix}"))
                    .when(scrolls, |block| {
                        block
                            .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                            .overflow_y_scroll()
                    })
                    .font_family(code.clone())
                    .children(
                        code_lines(text, syntax, &highlight)
                            .into_iter()
                            .map(|line| div().child(line)),
                    )
                    .into_any_element(),
            }
        };
        let body = match view.sections.as_slice() {
            [only] => block(&only.text, &view.name, String::new(), true),
            sections => div()
                .id("show-ddl-sections")
                .debug_selector(|| "show-ddl-sections".into())
                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_MD))
                .children(sections.iter().enumerate().map(|(at, section)| {
                    let copy_workspace = cx.entity().downgrade();
                    let ready = matches!(&section.text, DdlText::Ready(text) if !text.is_empty());
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(layout::SPACE_XS))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(layout::SPACE_MD))
                                .child(section_label(t, &section.title))
                                .child(
                                    div()
                                        .debug_selector(move || format!("show-ddl-copy-{at}"))
                                        .child(
                                            button(
                                                format!("show-ddl-copy-{at}"),
                                                "Copy",
                                                Tone::Quiet,
                                                Control::Compact,
                                                t,
                                            )
                                            .disabled(!ready)
                                            .on_click(
                                                move |_, _, cx| {
                                                    _ = copy_workspace.update(
                                                        cx,
                                                        |workspace, cx| {
                                                            workspace.copy_ddl_section(at, cx);
                                                        },
                                                    );
                                                },
                                            ),
                                        ),
                                ),
                        )
                        .child(block(
                            &section.text,
                            &section.title,
                            format!("-{at}"),
                            false,
                        ))
                }))
                .into_any_element(),
        };
        let close_workspace = cx.entity().downgrade();
        let copy_workspace = close_workspace.clone();

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
                        .child(section_label(t, &view.heading()))
                        .child(div().text_size(text_size).child(body))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "show-ddl",
                                        1,
                                        button(
                                            "copy-shown-ddl",
                                            "Copy",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .disabled(!copyable)
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = copy_workspace.update(cx, |workspace, cx| {
                                                    workspace.copy_shown_ddl(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "show-ddl",
                                        0,
                                        button(
                                            "close-shown-ddl",
                                            "Close",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = close_workspace.update(cx, |workspace, cx| {
                                                    workspace.close_ddl_view(cx);
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

    /// What Quit asks while a transaction is open: quitting ends every
    /// session, and the server rolls back what an open transaction ran.
    pub(crate) fn render_quit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.quit_prompt {
            return None;
        }
        let open = self.open_transactions();
        if open.is_empty() {
            return None;
        }
        Some(self.question(
            "quit",
            "Quit",
            format!(
                "A transaction is open on {}. Quitting ends it, and the server rolls back \
                 everything it ran.",
                open.join(", ")
            ),
            [
                ("cancel-quit", "Cancel", Tone::Quiet, |workspace, _, cx| {
                    workspace.cancel_quit(cx);
                }),
                ("confirm-quit", "Quit", Tone::Danger, |workspace, _, cx| {
                    workspace.confirm_quit(cx)
                }),
            ],
            cx,
        ))
    }

    /// What the Auto dialog's button `choice`, counted from the right, ends
    /// the transaction with; `None` for Cancel. Commit is not offered over a
    /// failed transaction, which moves the others one to the right.
    fn end_transaction_choice(&self, choice: usize) -> Option<&'static str> {
        let failed = self
            .profile()
            .is_some_and(crate::session::Profile::transaction_failed);
        let verbs: &[&'static str] = match failed {
            true => &["ROLLBACK"],
            false => &["COMMIT", "ROLLBACK"],
        };
        verbs.get(choice).copied()
    }

    /// What turning auto-commit back on asks while a transaction is open:
    /// commit it, roll it back, or leave it, with the statements it has run
    /// behind a toggle. Drawn only while the transaction is still open under
    /// a switch still reading Manual, so one that ended under the prompt
    /// draws nothing.
    pub(crate) fn render_end_transaction(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let prompt = profile.session.auto_commit_prompt.as_ref()?;
        if !profile.in_transaction || profile.auto_commit {
            return None;
        }
        let syntax = profile.config.engine().syntax();
        let statements = &profile.transaction_statements;
        let shown = prompt.statements_shown;
        // A failed transaction only rolls back: a `COMMIT` would roll it back
        // under a button that said otherwise.
        let failed = profile.transaction_failed();
        let at = |verb| {
            (0..3)
                .find(|choice| self.end_transaction_choice(*choice) == verb)
                .unwrap_or_default()
        };
        let highlight = cx.theme().highlight_theme.clone();
        let code = fonts(cx).editor.clone();
        let cancel = cx.entity().downgrade();
        let rollback = cancel.clone();
        let commit = cancel.clone();
        let toggle = cancel.clone();

        let list = shown.then(|| {
            div()
                .id("end-transaction-statement-list")
                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                .overflow_y_scroll()
                .p(px(layout::SPACE_SM))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .rounded(px(layout::RADIUS_CONTROL))
                .bg(t.surface)
                .font_family(code)
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text)
                .children(statements.iter().enumerate().map(|(i, statement)| {
                    div()
                        .debug_selector(move || format!("end-transaction-statement-{i}"))
                        .children(
                            code_lines(statement, syntax, &highlight)
                                .into_iter()
                                .map(|line| div().child(line)),
                        )
                }))
        });

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
                        .child(section_label(t, "Auto-commit"))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(match failed {
                                    true => {
                                        "The transaction open on this connection's own session \
                                         failed, so it can only be rolled back. Turning \
                                         auto-commit on ends it."
                                    }
                                    false => {
                                        "A transaction is open on this connection's own \
                                         session. Turning auto-commit on ends it: commit what \
                                         it ran, or roll it back."
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id("end-transaction-statements")
                                .debug_selector(|| "end-transaction-statements".into())
                                .flex()
                                .items_center()
                                .gap(px(layout::SPACE_XS))
                                .cursor_pointer()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(row_icon(
                                    t,
                                    match shown {
                                        true => icon::CHEVRON_DOWN,
                                        false => icon::CHEVRON_RIGHT,
                                    },
                                ))
                                .child(format!(
                                    "Statements in this transaction ({})",
                                    statements.len()
                                ))
                                .on_click(move |_, _, cx| {
                                    _ = toggle.update(cx, |workspace, cx| {
                                        workspace.toggle_auto_commit_statements(cx);
                                    });
                                }),
                        )
                        .children(list)
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "end-transaction",
                                        at(None),
                                        button(
                                            "cancel-end-transaction",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = cancel.update(cx, |workspace, cx| {
                                                    workspace.cancel_auto_commit_prompt(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "end-transaction",
                                        at(Some("ROLLBACK")),
                                        button(
                                            "rollback-end-transaction",
                                            "Roll back",
                                            Tone::Danger,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                _ = rollback.update(cx, |workspace, cx| {
                                                    workspace.end_transaction_for_auto(
                                                        "ROLLBACK", window, cx,
                                                    );
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .children((!failed).then(|| {
                                    self.choice(
                                        "end-transaction",
                                        at(Some("COMMIT")),
                                        button(
                                            "commit-end-transaction",
                                            "Commit",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                _ = commit.update(cx, |workspace, cx| {
                                                    workspace.end_transaction_for_auto(
                                                        "COMMIT", window, cx,
                                                    );
                                                });
                                            },
                                        ),
                                        t,
                                    )
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What making a connection visible to agents asks first: which hold the
    /// engine has while the profile is Read-only, and that rows an agent reads
    /// leave this machine. Nothing here asks a server; the facts are static.
    /// Nothing is drawn unless the prompt's profile is still in the list.
    pub(crate) fn render_agent_visibility(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let id = self.agent_visibility_prompt.as_deref()?;
        let profile = self.profiles.iter().find(|profile| profile.id == id)?;
        let facts = visibility_facts(
            profile.config.engine(),
            profile.mode,
            self.agent_visibility_blocks_commits,
        );
        let cancel_workspace = cx.entity().downgrade();
        let confirm_workspace = cancel_workspace.clone();

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
                        .child(section_label(t, "Agent access"))
                        // Not in the label: it is upper-cased there, and a
                        // connection's name is spelt the way its owner wrote it.
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text)
                                .child(format!("Make {} visible to agents", profile.name)),
                        )
                        .children(facts.into_iter().map(|fact| {
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(fact)
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "agent-visibility",
                                        1,
                                        button(
                                            "cancel-agent-visibility",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = cancel_workspace.update(cx, |workspace, cx| {
                                                    workspace.cancel_agent_visibility(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "agent-visibility",
                                        0,
                                        button(
                                            "confirm-agent-visibility",
                                            "Make visible",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = confirm_workspace.update(
                                                    cx,
                                                    |workspace, cx| {
                                                        workspace.confirm_agent_visibility(cx);
                                                    },
                                                );
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

    /// Confirmation before switching `execute_sql` writes to Auto. Cancel is
    /// the default choice so a held Enter never grants automatic writes.
    pub(crate) fn render_agent_writes_auto(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.agent_writes_auto_prompt {
            return None;
        }
        let t = popup_theme(cx);
        let cancel_workspace = cx.entity().downgrade();
        let confirm_workspace = cancel_workspace.clone();

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
                        .child(section_label(t, "AI Features"))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text)
                                .child("Allow automatic writes?"),
                        )
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(
                                    "Agents will be able to change data without asking for your \
                                     approval first.",
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(
                                    "Read-only connections still refuse writes, and DROP, \
                                     TRUNCATE, and any DELETE remain refused.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "agent-writes-auto",
                                        1,
                                        button(
                                            "cancel-agent-writes-auto",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = cancel_workspace.update(cx, |workspace, cx| {
                                                    workspace.cancel_agent_writes_auto(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "agent-writes-auto",
                                        0,
                                        button(
                                            "confirm-agent-writes-auto",
                                            "Allow automatic writes",
                                            Tone::Danger,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = confirm_workspace.update(
                                                    cx,
                                                    |workspace, cx| {
                                                        workspace.confirm_agent_writes_auto(cx);
                                                    },
                                                );
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

    /// The titlebar MCP popover: status, waiting calls with Review, per-tool
    /// and per-class policy rows, registration command, Activity log, AI
    /// settings and Disconnect agents.
    pub(crate) fn render_agent_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::agent::policy::PolicyRow;

        if !self.agent_popover_open {
            return None;
        }
        let mark = self.agent_mark_state()?;
        let t = popup_theme(cx);
        let mono = fonts(cx).editor.clone();
        let waiting = self.agent.calls.len();
        let status_label = match mark {
            ui::AgentMark::Listening => "MCP server listening",
            ui::AgentMark::Connected { .. } => "MCP server connected",
        };
        let reachable: Vec<String> = self
            .agent_reachable_profiles()
            .map(|profile| profile.name.clone())
            .collect();
        let reachable_text = if reachable.is_empty() {
            "Reachable: none".to_string()
        } else {
            format!("Reachable: {}", reachable.join(", "))
        };
        let command = self
            .agent_registration_command()
            .unwrap_or_else(|| "claude mcp add dbdelve -- dbdelve mcp".to_string());

        let policy_row_el = |row: PolicyRow, indented: bool, cx: &mut Context<Self>| {
            let current = self.settings.agent_policy.get(row);
            let choices = row.choices();
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(layout::SPACE_SM))
                .when(indented, |el| el.pl(px(layout::SPACE_MD)))
                .child(
                    div()
                        .font_family(if indented {
                            fonts(cx).chrome.clone()
                        } else {
                            mono.clone()
                        })
                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                        .text_color(if indented { t.text_muted } else { t.text })
                        .child(row.label()),
                )
                .child(
                    ui::segmented(t).children(choices.iter().copied().map(|level| {
                        let selected = level == current;
                        let selector = format!("policy-{}-{}", row.slug(), level.slug());
                        div().debug_selector(move || selector.clone()).child(
                            ui::segmented_choice(
                                ("agent-policy", row as usize * 3 + level as usize),
                                level.label(),
                                selected,
                                true,
                                t,
                            )
                            .on_click(cx.listener(
                                move |workspace, _: &gpui::ClickEvent, _, cx| {
                                    workspace.set_agent_policy(row, level, cx);
                                },
                            )),
                        )
                    })),
                )
        };

        let schema_rows: Vec<_> = PolicyRow::SCHEMA_TOOLS
            .into_iter()
            .map(|row| policy_row_el(row, false, cx))
            .collect();
        let sql_rows: Vec<_> = PolicyRow::SQL_CLASSES
            .into_iter()
            .map(|row| policy_row_el(row, true, cx))
            .collect();

        Some(
            div()
                .id("agent-popover-backdrop")
                .absolute()
                .top(px(layout::TITLEBAR_HEIGHT))
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|workspace, _, _, cx| {
                        workspace.close_agent_popover(cx);
                    }),
                )
                .child(
                    div()
                        .id("agent-popover")
                        .debug_selector(|| "agent-popover".into())
                        .absolute()
                        .top(px(layout::SPACE_XS))
                        .right(px(layout::SPACE_MD))
                        .w(px(380.))
                        .max_h(gpui::relative(0.92))
                        .overflow_y_scroll()
                        .border_1()
                        .border_color(t.border_strong)
                        .rounded(px(layout::RADIUS_CARD))
                        .map(|card| ui::raised_card(card, t))
                        .p(px(layout::SPACE_MD))
                        .flex()
                        .flex_col()
                        .gap(px(layout::SPACE_MD))
                        // Stop clicks inside the card from dismissing it via
                        // the backdrop handler.
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(layout::SPACE_SM))
                                        .child(
                                            icon(icon::AGENT_MARK)
                                                .size(px(layout::chrome(layout::ICON_SIZE)))
                                                .text_color(t.accent),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(layout::chrome(layout::TEXT_MD)))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(t.text)
                                                .child("AI Features"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px(px(layout::SPACE_SM))
                                        .py(px(2.))
                                        .rounded_full()
                                        .bg(t.surface)
                                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                                        .text_color(t.accent)
                                        .child(format!("\u{2022} {status_label}")),
                                ),
                        )
                        .when(waiting > 0, |card| {
                            let label = if waiting == 1 {
                                "Claude Code is attached \u{b7} 1 call waiting for you".to_string()
                            } else {
                                format!(
                                    "Claude Code is attached \u{b7} {waiting} calls waiting for you"
                                )
                            };
                            card.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(layout::SPACE_SM))
                                    .px(px(layout::SPACE_SM))
                                    .py(px(layout::SPACE_SM))
                                    .rounded(px(layout::RADIUS_CONTROL))
                                    .bg(t.surface)
                                    .border_1()
                                    .border_color(t.accent)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                                            .text_color(t.text)
                                            .child(label),
                                    )
                                    .child(
                                        button(
                                            "agent-popover-review",
                                            "Review",
                                            Tone::Primary,
                                            Control::Compact,
                                            t,
                                        )
                                        .on_click(
                                            cx.listener(|workspace, _, _, cx| {
                                                workspace.close_agent_popover(cx);
                                            }),
                                        ),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                                        .text_color(t.text_muted)
                                        .child(reachable_text),
                                )
                                .child(
                                    button(
                                        "agent-popover-manage",
                                        "Manage in Settings",
                                        Tone::Quiet,
                                        Control::Compact,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        |workspace, _, window, cx| {
                                            workspace.open_ai_settings(window, cx);
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .pt(px(layout::SPACE_XS))
                                .border_t_1()
                                .border_color(t.border)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(section_label(t, "Tools"))
                                        .child(
                                            div()
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .text_color(t.text_faint)
                                                .child("Deny \u{b7} Ask first \u{b7} Auto"),
                                        ),
                                )
                                .children(schema_rows)
                                .child(
                                    div()
                                        .pt(px(layout::SPACE_XS))
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .font_family(mono.clone())
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .text_color(t.text)
                                                .child("execute_sql"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .text_color(t.text_faint)
                                                .child(
                                                    "Sorted by what each statement does. Writes \
                                                     ask by default; Auto needs your confirmation.",
                                                ),
                                        ),
                                )
                                .children(sql_rows),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .pt(px(layout::SPACE_XS))
                                .border_t_1()
                                .border_color(t.border)
                                .child(section_label(t, "Connect an agent"))
                                .child(div().flex().flex_wrap().items_center().child(
                                    ui::segmented(t).flex_wrap().children(
                                        crate::agent::shim::AgentClient::ALL.map(|choice| {
                                            let selector =
                                                format!("popover-connect-{}", choice.slug());
                                            div().debug_selector(move || selector.clone()).child(
                                                ui::segmented_icon_choice(
                                                    ("popover-agent-client", choice as usize),
                                                    choice.icon(),
                                                    choice.label(),
                                                    choice == self.agent_client,
                                                    true,
                                                    t,
                                                )
                                                .on_click(cx.listener(
                                                    move |workspace, _, _, cx| {
                                                        workspace.set_agent_client(choice, cx);
                                                    },
                                                )),
                                            )
                                        }),
                                    ),
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .items_start()
                                        .gap(px(layout::SPACE_SM))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .px(px(layout::SPACE_SM))
                                                .py(px(layout::SPACE_XS))
                                                .rounded(px(layout::RADIUS_CONTROL))
                                                .bg(t.surface)
                                                .border_1()
                                                .border_color(t.border)
                                                .font_family(mono)
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .text_color(t.text)
                                                .child(command),
                                        )
                                        .child(
                                            button(
                                                "agent-popover-copy",
                                                "Copy",
                                                Tone::Quiet,
                                                Control::Compact,
                                                t,
                                            )
                                            .on_click(
                                                cx.listener(|workspace, _, _, cx| {
                                                    workspace.copy_agent_registration(cx);
                                                }),
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(layout::chrome(layout::TEXT_XS)))
                                        .text_color(t.text_faint)
                                        .child(self.agent_client.note()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(layout::SPACE_SM))
                                .pt(px(layout::SPACE_XS))
                                .border_t_1()
                                .border_color(t.border)
                                .child(
                                    button(
                                        "agent-popover-activity",
                                        "Activity log",
                                        Tone::Quiet,
                                        Control::Compact,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        |workspace, _, _, cx| {
                                            workspace.agent_popover_open = false;
                                            workspace.show_agent_activity(cx);
                                        },
                                    )),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(layout::SPACE_SM))
                                        .child(
                                            button(
                                                "agent-popover-settings",
                                                "AI settings",
                                                Tone::Quiet,
                                                Control::Compact,
                                                t,
                                            )
                                            .on_click(
                                                cx.listener(|workspace, _, window, cx| {
                                                    workspace.open_ai_settings(window, cx);
                                                }),
                                            ),
                                        )
                                        .child(
                                            div()
                                                .debug_selector(|| {
                                                    "agent-popover-disconnect".into()
                                                })
                                                .child(
                                                    button(
                                                        "agent-popover-disconnect",
                                                        "Disconnect agents",
                                                        Tone::Danger,
                                                        Control::Compact,
                                                        t,
                                                    )
                                                    .on_click(cx.listener(|workspace, _, _, cx| {
                                                        workspace.agent_popover_open = false;
                                                        workspace.disconnect_agents(cx);
                                                    })),
                                                ),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What agents did this session, newest first: one block per call with its
    /// tool, connection, verdict, row counts, time and statement, and the full
    /// error of a call that failed. Never a row. Drawn below the titlebar, as
    /// an approval is, so the mark stays reachable.
    ///
    /// ponytail: every entry is laid out while the dialog is open. The upgrade
    /// is a virtual list, if five hundred entries ever prove slow.
    pub(crate) fn render_agent_audit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.agent_audit_open {
            return None;
        }
        let t = popup_theme(cx);
        let code = fonts(cx).editor.clone();
        let now = agent::unix_millis();
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let close_workspace = cx.entity().downgrade();
        let entries: Vec<Div> = self
            .agent
            .log
            .iter()
            .map(|entry| {
                let mut lines = vec![
                    div()
                        .text_color(t.text)
                        .child(super::agent_log_label(entry, now)),
                ];
                if let Some(verdict) = &entry.verdict {
                    lines.push(div().text_color(t.text_muted).child(verdict.line()));
                }
                if let (Some(shown), Some(total)) = (entry.shown, entry.total) {
                    let of = match total {
                        Total::Exact(total) => total.to_string(),
                        Total::AtLeast(total) => format!("at least {total}"),
                    };
                    let capped = if entry.capped() { ", capped" } else { "" };
                    lines.push(div().text_color(t.text_muted).child(format!(
                        "{shown} rows shown of {of}{capped}, {} ms",
                        entry.elapsed_ms
                    )));
                } else {
                    lines.push(
                        div()
                            .text_color(t.text_muted)
                            .child(format!("{} ms", entry.elapsed_ms)),
                    );
                }
                if let Some(sql) = &entry.sql {
                    lines.push(
                        div()
                            .debug_selector(|| "agent-audit-sql".into())
                            .font_family(code.clone())
                            .text_color(t.text_muted)
                            .child(sql.clone()),
                    );
                }
                if let Some(error) = &entry.error {
                    lines.push(div().text_color(t.text_muted).child(error.clone()));
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_XS))
                    .children(lines)
            })
            .collect();

        Some(
            div()
                .absolute()
                .top(px(layout::TITLEBAR_HEIGHT))
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Agent activity"))
                        .child(
                            div()
                                .id("agent-audit-list")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_MD))
                                .text_size(text_size)
                                .when(entries.is_empty(), |list| {
                                    list.child(
                                        div()
                                            .debug_selector(|| "agent-audit-empty".into())
                                            .text_color(t.text_muted)
                                            .child("No agent calls in this session."),
                                    )
                                })
                                .children(entries),
                        )
                        .child(
                            div().flex().justify_end().child(
                                self.choice(
                                    "agent-audit",
                                    0,
                                    button(
                                        "close-agent-audit",
                                        "Close",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = close_workspace.update(cx, |workspace, cx| {
                                                workspace.close_agent_audit(cx);
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

    /// What an agent asks to run: the connection by the profile's own name, the
    /// facts the gate and the profile give, and the statement exactly as the
    /// agent sent it, in a block that scrolls. Deny is the ring's only stop;
    /// Approve is a click, off until [`agent::ARM_DELAY`] has passed since this
    /// was drawn, and never a stop for a key. The profile is read by id, never
    /// by anything the agent sent. Nothing is drawn unless the oldest waiting
    /// call still has its profile.
    pub(crate) fn render_agent_approval(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let call = self.front_approval()?;
        let id = call.id;
        let tool = call.tool;
        let profile = call.profile_id.as_deref().and_then(|profile_id| {
            self.profiles
                .iter()
                .find(|profile| profile.id == profile_id)
        });
        if call.profile_id.is_some() && profile.is_none() {
            return None;
        }
        let source_key = match &call.approval {
            agent::Approval::Sql(approval) => approval.sql.clone(),
            agent::Approval::Tool(approval) => agent::tool_approval_source(tool, &approval.args),
        };
        // What Approve will compare the pending call with: the source string,
        // never the marked text drawn from it. Written only on a change, so a
        // frame costs one comparison.
        {
            let mut shown = self.approval_shown.borrow_mut();
            if shown
                .as_ref()
                .is_none_or(|(shown_id, src)| *shown_id != id || *src != source_key)
            {
                *shown = Some((id, source_key));
            }
        }
        // The block's scroll offset, fresh for each call.
        let scroll = {
            let mut kept = self.approval_scroll.borrow_mut();
            match kept.as_ref() {
                Some((kept_id, handle)) if *kept_id == id => handle.clone(),
                _ => {
                    let handle = gpui::ScrollHandle::new();
                    *kept = Some((id, handle.clone()));
                    handle
                }
            }
        };
        let code = fonts(cx).editor.clone();
        let marker = approval_marker(t);
        let fixed = || div().flex_shrink_0();
        let waiting = self.agent.calls.len().saturating_sub(1);
        let deny_workspace = cx.entity().downgrade();
        let approve_workspace = deny_workspace.clone();
        let armed = self.agent.armed == Some(id);

        let (header_row, facts_el, body_label, body_block, footer_size): (
            gpui::Div,
            gpui::Div,
            &'static str,
            AnyElement,
            Option<String>,
        ) = match &call.approval {
            agent::Approval::Sql(approval) => {
                let profile = profile?;
                let engine = profile.config.engine();
                let lines = code_lines_visible(
                    &approval.sql,
                    engine.syntax(),
                    &cx.theme().highlight_theme,
                    marker,
                );
                let size = size_fact(&approval.sql);
                let facts = approval_facts(
                    engine,
                    profile.mode,
                    &approval.verdict,
                    &approval.sql,
                    approval.locks_the_file,
                    approval.config.agent_reads_are_held(),
                )
                .into_iter()
                .filter(|fact| *fact != size)
                .map(|fact| div().child(fact));
                let header = fixed()
                    .flex()
                    .gap(px(layout::SPACE_SM))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .child(div().text_color(t.text).child(profile.name.clone()))
                    .child(div().text_color(t.text_muted).child(engine.label()));
                let facts_div = fixed()
                    .debug_selector(|| "agent-approval-facts".into())
                    .flex()
                    .flex_col()
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .children(facts);
                let block = div()
                    .id("agent-approval-sql")
                    .debug_selector(|| "agent-approval-sql".into())
                    .track_scroll(&scroll)
                    .overflow_y_scroll()
                    .min_h(px(0.))
                    .p(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.surface)
                    .font_family(code)
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text)
                    .children(lines.into_iter().enumerate().map(|(index, line)| {
                        div()
                            .debug_selector(move || format!("agent-approval-line-{index}"))
                            .child(line)
                    }));
                (
                    header,
                    facts_div,
                    "Statement from the agent, exactly as sent",
                    block.into_any_element(),
                    Some(size),
                )
            }
            agent::Approval::Tool(approval) => {
                let header = match profile {
                    Some(profile) => fixed()
                        .flex()
                        .gap(px(layout::SPACE_SM))
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .child(div().text_color(t.text).child(profile.name.clone()))
                        .child(
                            div()
                                .text_color(t.text_muted)
                                .child(profile.config.engine().label()),
                        ),
                    None => fixed()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text)
                        .child("Visible connections"),
                };
                let facts_div = fixed()
                    .debug_selector(|| "agent-approval-facts".into())
                    .flex()
                    .flex_col()
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(format!("Tool: {}", tool.name()))
                    .child("Held because this tool's AI Features policy is set to Ask.");
                let arg_rows = agent::tool_approval_args(tool, &approval.args);
                let block = div()
                    .id("agent-approval-sql")
                    .debug_selector(|| "agent-approval-tool-args".into())
                    .track_scroll(&scroll)
                    .overflow_y_scroll()
                    .min_h(px(0.))
                    .p(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.surface)
                    .font_family(code)
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text)
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_XS))
                    .when(arg_rows.is_empty(), |box_el| {
                        box_el.child(div().text_color(t.text_muted).child("(no arguments)"))
                    })
                    .children(arg_rows.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(t.text_muted)
                                    .child(format!("{label}:")),
                            )
                            .child(div().min_w_0().child(ui::visible(&value).display))
                    }));
                (
                    header,
                    facts_div,
                    "Tool arguments from the agent",
                    block.into_any_element(),
                    None,
                )
            }
        };

        Some(
            div()
                // Below the titlebar rather than over it: the agent mark's
                // waiting count and its turn-off item must stay clickable
                // while an approval waits, and the window controls live in
                // that strip too.
                .absolute()
                .top(px(layout::TITLEBAR_HEIGHT))
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .py(px(layout::SPACE_LG))
                .child(
                    dialog(t)
                        .w(px(layout::APPROVAL_DIALOG_WIDTH))
                        .max_w(gpui::relative(0.9))
                        .max_h_full()
                        .child(fixed().child(section_label(t, "Agent")))
                        .child(header_row)
                        .child(facts_el)
                        .child(
                            fixed()
                                .debug_selector(|| "agent-approval-label".into())
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(body_label),
                        )
                        // Nothing on this surface moves on its own, so the block
                        // has no smooth-scroll modifier and its scrollbar is
                        // always drawn rather than faded in: a statement that
                        // goes on below the fold shows that it does. One
                        // element per line, so none is left out of a long
                        // statement.
                        .child(
                            div()
                                .relative()
                                .flex()
                                .flex_col()
                                .min_h(px(0.))
                                .max_h(px(layout::APPROVAL_TEXT_MAX_HEIGHT))
                                .child(body_block)
                                .child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .debug_selector(|| "agent-approval-scrollbar".into())
                                        .child(
                                            Scrollbar::vertical(&scroll)
                                                .id(("agent-approval-scrollbar", id.0))
                                                .mode(ScrollbarMode::Always),
                                        ),
                                ),
                        )
                        .when_some(footer_size, |card, size| {
                            card.child(
                                fixed()
                                    .debug_selector(|| "agent-approval-size".into())
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .text_color(t.text_muted)
                                    .child(size),
                            )
                        })
                        .when(waiting > 0, |card| {
                            card.child(
                                fixed()
                                    .debug_selector(|| "agent-approval-waiting".into())
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .text_color(t.text_muted)
                                    .child(format!("{waiting} more waiting")),
                            )
                        })
                        .child(
                            fixed()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "agent-approval",
                                        0,
                                        button(
                                            "deny-agent-approval",
                                            "Deny",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = deny_workspace.update(cx, |workspace, cx| {
                                                    workspace.deny_call(id, cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                // Outside the ring, and with an id of this call
                                // alone: a press that began on one call's button
                                // cannot finish on the next call's.
                                .child(
                                    div()
                                        .debug_selector(|| "agent-approval-approve".into())
                                        .child(
                                            button_inked(
                                                format!("approve-write-{}", id.0),
                                                "Approve",
                                                Tone::Quiet,
                                                Control::Standard,
                                                approve_ink(t, armed),
                                            )
                                            // A key cannot move onto it, so a key
                                            // cannot press it.
                                            .tab_stop(false)
                                            .disabled(!armed)
                                            .on_click(
                                                move |event, _, cx| {
                                                    if !approves(event) {
                                                        return;
                                                    }
                                                    _ = approve_workspace.update(
                                                        cx,
                                                        |workspace, cx| {
                                                            workspace.approve_call(id, cx);
                                                        },
                                                    );
                                                },
                                            ),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What Delete asks before a saved query's file goes, since nothing
    /// brings it back, or before a folder holding anything goes.
    pub(crate) fn render_delete_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let session = &self.profile()?.session;
        let deleting = session.pending_delete.clone()?;
        let (heading, question) = match &deleting {
            PendingDelete::Query(query)
                if session
                    .deleted_with(query)
                    .is_some_and(|path| self.tab_on_file(&path).is_some()) =>
            {
                let name = crate::explorer::query_file_name(query);
                (
                    "Delete query",
                    format!("Delete {name}? Its tab is closed too. This cannot be undone."),
                )
            }
            PendingDelete::Query(query) => (
                "Delete query",
                format!(
                    "Delete {}? This cannot be undone.",
                    crate::explorer::query_file_name(query)
                ),
            ),
            PendingDelete::Folder(name, held) => (
                "Delete folder",
                format!(
                    "Delete {name}? Only the folder goes: the {held} {} in it {} listed \
                     everywhere else.",
                    if *held == 1 { "item" } else { "items" },
                    if *held == 1 { "stays" } else { "stay" },
                ),
            ),
        };
        let cancel_workspace = cx.entity().downgrade();
        let delete_workspace = cancel_workspace.clone();

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
                        .child(section_label(t, heading))
                        .child(
                            div()
                                .debug_selector({
                                    let question = question.clone();
                                    move || format!("delete-question: {question}")
                                })
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(question),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "delete",
                                        1,
                                        button(
                                            "cancel-delete-query",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = cancel_workspace.update(cx, |workspace, cx| {
                                                    workspace.cancel_delete_query(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "delete",
                                        0,
                                        button(
                                            "confirm-delete-query",
                                            "Delete",
                                            Tone::Danger,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                _ = delete_workspace.update(cx, |workspace, cx| {
                                                    match deleting.clone() {
                                                        PendingDelete::Query(name) => workspace
                                                            .delete_saved_query(name, window, cx),
                                                        PendingDelete::Folder(name, _) => {
                                                            workspace.delete_folder(name, cx)
                                                        }
                                                    }
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
        // upgrade arm changes the mode itself, mid-prompt). With the plain
        // gate, not `gate_run`: the opt-in decides whether a prompt goes up,
        // and one already up that it later waved through would vanish with
        // its statement neither run nor cancelled.
        let stop = sql::gate(&pending.verdict, profile.mode, &profile.confirmed)?;
        let name = profile.name.clone();
        let current_mode = profile.mode.label();
        let syntax = profile.config.engine().syntax();
        let sql = pending
            .resume
            .as_ref()
            .map(|resume| code_lines(&resume.sql, syntax, &cx.theme().highlight_theme));
        let code = fonts(cx).editor.clone();
        let dont_ask = pending.dont_ask;
        let own_run = pending.own_run;

        let (title, message, confirm, tone) = match stop {
            Stop::Upgrade(needed) => (
                "Mode",
                // `Destructive::Unreadable` only reaches `Upgrade` where the
                // server's read-only setting cannot be relied on to stop a
                // write dbdelve cannot parse (`Engine::unreadable_reason`), so
                // the reason is worth spelling out rather than reading like an
                // ordinary mode shortfall.
                if pending
                    .verdict
                    .destructive
                    .contains(&Destructive::Unreadable)
                {
                    format!(
                        "{name} is in {current_mode} mode. dbdelve can't parse this, and {}, \
                         so it needs {}.",
                        profile.config.engine().unreadable_reason(),
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
                                .font_family(code)
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text)
                                .children(sql.into_iter().map(|line| div().child(line)))
                        }))
                        // An unreadable statement's box turns on the app-wide
                        // setting, never this connection's silenced list: it
                        // silences every future typo, not one kind, so it is a
                        // setting the panel shows and can take back.
                        .children(
                            match stop {
                                Stop::Confirm(kind) if kind.suppressible() => {
                                    Some(format!("Don't ask again for {} on {name}", kind.label()))
                                }
                                Stop::RunOnce if own_run => Some(
                                    "Run statements dbdelve can't parse without asking".to_string(),
                                ),
                                _ => None,
                            }
                            .map(|label| {
                                self.choice(
                                    "pending-run",
                                    2,
                                    Checkbox::new("dont-ask-again")
                                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                                        .font_weight(FontWeight::BOLD)
                                        .label(label)
                                        .checked(dont_ask)
                                        .on_click(move |_, _, cx| {
                                            _ = tick.update(cx, |workspace, cx| {
                                                workspace.toggle_dont_ask(cx);
                                            });
                                        }),
                                    t,
                                )
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "pending-run",
                                        1,
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
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "pending-run",
                                        0,
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
                                        t,
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
                                    self.choice(
                                        "queue-failure",
                                        1,
                                        button(
                                            "stop-queue",
                                            "Stop",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = stop.update(cx, |workspace, cx| {
                                                    workspace.stop_queue(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "queue-failure",
                                        0,
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
                                        t,
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
            .and_then(|tab| tab.last_query.as_deref())
            .map(|sql| {
                code_lines(
                    sql,
                    profile.config.engine().syntax(),
                    &cx.theme().highlight_theme,
                )
            });
        let code = fonts(cx).editor.clone();

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
                                        .font_family(code)
                                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                                        .text_color(t.text)
                                        .children(sql.into_iter().map(|line| div().child(line))),
                                )
                        }))
                        .children(editing.then(|| {
                            self.choice(
                                "stale-edit",
                                3,
                                Checkbox::new("dont-ask-stale")
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .font_weight(FontWeight::BOLD)
                                    .label("Don't ask again for this connection")
                                    .checked(dont_ask)
                                    .on_click(move |_, _, cx| {
                                        _ = tick.update(cx, |workspace, cx| {
                                            workspace.toggle_stale_dont_ask(cx);
                                        });
                                    }),
                                t,
                            )
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "stale-edit",
                                        if editing { 2 } else { 1 },
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
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "stale-edit",
                                        usize::from(editing),
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
                                        t,
                                    ),
                                )
                                .children(editing.then(|| {
                                    self.choice(
                                        "stale-edit",
                                        0,
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
                                        ),
                                        t,
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
    /// It stays up until the batch succeeds. On a target whose batch has no
    /// transaction `execute_sql` clears the grid as it starts and the pending
    /// edits go with it, so after a failure this is the only remaining copy of
    /// what was attempted; elsewhere the edits stay too, and the panel carries
    /// the server's message.
    pub(crate) fn render_apply_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let code = fonts(cx).editor.clone();
        let profile = self.profile()?;
        let review = profile.session.apply_review.as_ref()?;
        if !review.covers(profile.session.active) {
            return None;
        }
        let scope = review.tab.scroll_scope(&profile.id);
        // A send that keeps the draft says why it failed in the review itself
        // (`ApplyReview::error`); a target that clears the grid ends the tab in
        // `Failed`, and the batch is the only thing this tab can have run while
        // the panel is open, so that failure is this batch's.
        //
        // A grant review runs on sessions of its own and never through the
        // tab: what that tab's own statement came to is not its failure, nor
        // is its running the review's.
        let grants = review.grants.as_ref();
        let error = review
            .error
            .clone()
            .or_else(|| match profile.session.active_query() {
                Some(QueryState::Failed(error)) if grants.is_none() => Some(error.message.clone()),
                _ => None,
            });
        let running = match grants {
            Some(run) => run.running.is_some(),
            None => matches!(
                profile.session.active_query(),
                Some(QueryState::Running { .. })
            ),
        };
        let lines = code_lines(
            &review.sql,
            profile.config.engine().syntax(),
            &cx.theme().highlight_theme,
        );
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
                                .children(lines.into_iter().map(|line| div().child(line))),
                        )
                        .children((!review.notes.is_empty()).then(|| {
                            div()
                                .id("apply-review-notes")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_SM))
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .children(review.notes.iter().map(|note| div().child(note.clone())))
                        }))
                        .children(error.map(|message| {
                            div()
                                .id("apply-review-error")
                                .debug_selector(|| "apply-review-error".into())
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
                                    self.choice(
                                        "apply-review",
                                        1,
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
                                        t,
                                    ),
                                )
                                .child(
                                    // Quiet while it runs, because the library's
                                    // disabled fill is the only thing that
                                    // dims and our pinned label would stay
                                    // bright over it.
                                    self.choice(
                                        "apply-review",
                                        0,
                                        button(
                                            "run-apply",
                                            if running && grants.is_some() {
                                                "Running…"
                                            } else {
                                                "Run"
                                            },
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
                                        t,
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
                engine_logo(t, profile.config.target(), &[behind_logo]).into_any_element()
            }
            None => row_icon(t, icon::DATABASE).into_any_element(),
        };
        // A hidden connection is drawn exactly as it always was; only a visible
        // one gains the marker, beside the logo that leads its name.
        let active_logo = match self.profile() {
            Some(profile) if profile.agent_visible => div()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .child(active_logo)
                .child(row_icon(t, icon::AGENT_VISIBLE))
                .into_any_element(),
            _ => active_logo,
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
    /// active schema, each a step after the connection that opens its picker,
    /// and the way to read what they hold again.
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
        if profile.config.target().switches_database() {
            let database = profile
                .active_database()
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
        if profile.config.target().picks_schema() {
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
        steps.push(self.render_refresh_schema(profile, t).into_any_element());
        steps
    }

    /// Rename and delete, revealed by hovering a row grouped as
    /// `project-row-{index}`, and held in view while the keyboard is on one
    /// of them or the delete is armed.
    pub(super) fn project_actions(
        &self,
        index: usize,
        name: &str,
        window: &Window,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let workspace = cx.entity().downgrade();
        let rename_workspace = workspace.clone();
        let rename_name = name.to_string();
        let delete_name = name.to_string();
        let pending = self.pending_project_deletion.as_deref() == Some(name);
        let held = self.welcome_project_focus(index, cx);
        let focused = held.contains_focused(window, cx);
        div()
            .track_focus(&held)
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .when(!pending && !focused, |actions| {
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

    /// Dispatches what the shortcut does, so the two cannot come apart.
    fn render_refresh_schema(&self, profile: &Profile, t: Theme) -> Button {
        if profile.refreshing_schema() {
            return ui::control("refresh-schema", Tone::Quiet, Control::Compact)
                .w(px(Control::Compact.height()))
                .p_0()
                .loading(true)
                .child(
                    Spinner::new()
                        .with_size(px(layout::chrome(layout::ICON_SIZE)))
                        .color(Tone::Quiet.ink(t).into()),
                );
        }
        let hint = ui::chord_hint("refresh_connection", &self.settings.custom_keybindings);
        let tooltip = match hint.is_empty() {
            true => "Refresh schema".to_string(),
            false => format!("Refresh schema  {hint}"),
        };
        icon_button(
            "refresh-schema",
            icon::REFRESH,
            Tone::Quiet,
            Control::Compact,
            t,
        )
        .tooltip(tooltip)
        .on_click(|_, window, cx| window.dispatch_action(Box::new(RefreshConnection), cx))
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
        let shortcuts = profile.session.explorer_shortcuts.clone();
        let menu_shortcuts = shortcuts.clone();
        let profile_id = profile.id.clone();
        let menu_leaves = leaves.clone();
        let menu_workspace = workspace.clone();
        let syntax = profile.config.engine().syntax();
        let active_schema = profile.active_schema().map(gpui::SharedString::from);
        let menu_profile = profile.id.clone();
        // The database the rows are drawn from, which the connections tree's
        // rows name as theirs, so an export the menu offers is refused once a
        // switch has moved the connection.
        let menu_database = profile.listed_database();
        let drop_line = self.explorer_drop.clone();
        let open_queries: HashSet<String> = profile
            .session
            .queries
            .iter()
            .filter_map(|tab| profile.session.saved_query_of(tab).map(str::to_string))
            .collect();
        let files = ListedFiles::of(profile);
        let menu_files = files.clone();
        let renaming = profile.session.query_rename.as_ref().map(|rename| {
            (
                rename.row.clone(),
                rename.input.clone(),
                rename.error.clone(),
            )
        });
        let message = match &profile.catalog {
            CatalogState::Loading => Some(("Loading database objects…".to_string(), t.text_muted)),
            CatalogState::Failed(message) => Some((message.clone(), t.danger)),
            CatalogState::Loaded(catalog, _) if catalog.schemas.is_empty() => Some((
                crate::sidebar::empty_catalog_line(
                    profile.config.target().levels(),
                    catalog.left_out,
                ),
                t.text_muted,
            )),
            CatalogState::Loaded(..) => None,
        };
        let lists_schemas = matches!(
            &profile.catalog,
            CatalogState::Loaded(catalog, _) if !catalog.schemas.is_empty()
        );
        let show_sizes = self.settings.show_sizes;
        let tree = explorer_drawn(profile).then(|| {
            render_tree(
                &profile.session.explorer_tree,
                move |index, entry, _, _, cx| {
                    let t = *theme(cx);
                    let id = entry.item().id.as_str();
                    let leaf = leaves.get(id).copied();
                    let shortcut = shortcuts.get(id);
                    let missing = matches!(shortcut, Some(Shortcut::Member { missing: true, .. }));
                    let hidden = matches!(shortcut, Some(Shortcut::Member { hidden: true, .. }));
                    let hidden_relation = shortcut
                        .and_then(Shortcut::hidden_relation)
                        .map(|(object, kind)| (object.clone(), kind));
                    let query = saved_query(id)
                        .or_else(|| shortcut.and_then(Shortcut::query))
                        .map(str::to_string);
                    let file = files.paths.get(id).cloned();
                    let file_missing = files.missing.contains(id);
                    let pinned_group = id == PINNED_GROUP;
                    let group = id == FILES_GROUP || pinned_group;
                    // A shortcut is listed away from its schema, which is
                    // then all that tells two of the same name apart.
                    let schema = match shortcut {
                        Some(Shortcut::Member {
                            member: Member::Object(object),
                            ..
                        }) => Some(object.schema.clone()),
                        _ => None,
                    };
                    let label = entry.item().label.clone();
                    let drag_label = label.clone();
                    let line = drop_line
                        .as_ref()
                        .filter(|(row, _)| row.as_str() == id && cx.has_active_drag())
                        .map(|(_, after)| *after);
                    let schema_row = is_schema_row(id);
                    let active = schema_row && active_schema.as_ref() == Some(&label);
                    let open = query
                        .as_ref()
                        .is_some_and(|name| open_queries.contains(name))
                        || files.open.contains(id);
                    let marker = match (active, open) {
                        (true, _) => Some((t.accent, "Current schema")),
                        (_, true) => Some((t.text_muted, "Open in a tab")),
                        _ => None,
                    };
                    let rename = renaming.as_ref().filter(|(row, ..)| row.as_str() == id);
                    let matched = highlights
                        .get(entry.item().id.as_str())
                        .map_or(&[][..], Vec::as_slice);
                    // A schema is a row like any other, set apart by its icon
                    // and a little weight; a category only labels the run of
                    // objects under it, in the voice of the Files group.
                    let (label, row) = match (leaf, entry.depth()) {
                        // Should be there and is not: greyed, and opens nothing,
                        // but for a file, which asks to leave the list.
                        _ if missing || file_missing => (
                            highlighted(label, matched, t),
                            ListItem::new(index).text_color(t.text_muted),
                        ),
                        (Some(_), _) => (
                            highlighted(label, matched, t),
                            ListItem::new(index).text_color(t.text),
                        ),
                        _ if query.is_some()
                            || file.is_some()
                            || hidden_relation.is_some()
                            || matches!(shortcut, Some(Shortcut::Folder(_))) =>
                        {
                            (
                                highlighted(label, matched, t),
                                ListItem::new(index).text_color(t.text),
                            )
                        }
                        (None, 0) if !group => (
                            highlighted(label, matched, t),
                            ListItem::new(index)
                                .text_color(t.text)
                                .font_weight(FontWeight::MEDIUM),
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
                    let row_icon_path = match (leaf, shortcut) {
                        (Some(leaf), _) => object_icon(leaf.kind),
                        (
                            None,
                            Some(Shortcut::Member {
                                member: Member::Object(object),
                                ..
                            }),
                        ) => object_icon(object.kind),
                        _ if query.is_some() || file.is_some() || missing => {
                            saved_query_icon(syntax)
                        }
                        _ if entry.is_expanded() => icon::CHEVRON_DOWN,
                        _ => icon::CHEVRON_RIGHT,
                    };
                    let heading_icon = if id == FILES_GROUP {
                        Some(queries_group_icon(syntax))
                    } else if pinned_group {
                        Some(icon::PIN)
                    } else if schema_row {
                        Some(icon::SCHEMA)
                    } else if matches!(shortcut, Some(Shortcut::Folder(_))) {
                        Some(icon::FOLDER)
                    } else {
                        None
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
                                .relative()
                                .flex()
                                .items_center()
                                .gap(px(layout::SPACE_SM))
                                .min_w_0()
                                .flex_1()
                                .child(match missing || file_missing {
                                    true => {
                                        row_icon_tinted(t, row_icon_path, None).into_any_element()
                                    }
                                    false => row_icon(t, row_icon_path).into_any_element(),
                                })
                                .when_some(heading_icon, |row, path| row.child(row_icon(t, path)))
                                .child(match rename {
                                    Some((_, input, error)) => {
                                        rename_field(label, input, error.clone(), t)
                                    }
                                    // The tree gives every row the height of its
                                    // first, which this is once anything is pinned:
                                    // the empty text keeps it a line tall rather
                                    // than an icon tall.
                                    None if pinned_group => {
                                        div().flex_1().flex().items_center().child("\u{200B}")
                                    }
                                    None => div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(label),
                                })
                                .when_some(schema.filter(|_| rename.is_none()), |row, schema| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .whitespace_nowrap()
                                            .text_color(t.text_faint)
                                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                                            .font_weight(FontWeight::NORMAL)
                                            .child(schema),
                                    )
                                })
                                .when_some(
                                    crate::sidebar::shown_size(
                                        leaf.and_then(|leaf| leaf.size),
                                        show_sizes,
                                    ),
                                    |row, size| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .text_color(t.text_faint)
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .child(human_bytes(size)),
                                        )
                                    },
                                )
                                // In a slot as wide as the icon column, so the
                                // dot has something to hover and sits as far
                                // in from the right edge as the icons do from
                                // the left.
                                .when_some(
                                    marker.filter(|_| rename.is_none()),
                                    |row, (fill, tip)| {
                                        row.child(
                                            div()
                                                .id("explorer-marker")
                                                .flex_none()
                                                .size(px(layout::chrome(layout::ICON_SIZE)))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(dot(fill))
                                                .tooltip(move |window, cx| {
                                                    Tooltip::new(tip).build(window, cx)
                                                }),
                                        )
                                    },
                                )
                                .when_some(line, |row, after| {
                                    // Out through `ListItem`'s own `py_1` to
                                    // the row's edge.
                                    let edge = gpui::rems(-0.25);
                                    row.child(
                                        div()
                                            .absolute()
                                            .left_0()
                                            .right_0()
                                            .h(px(2.))
                                            .rounded(px(1.))
                                            .bg(t.accent)
                                            .map(|line| match after {
                                                true => line.bottom(edge),
                                                false => line.top(edge),
                                            }),
                                    )
                                }),
                        )
                        .debug_selector(move || format!("explorer-row-{index}"))
                        .when_some(file.clone(), |row, path| {
                            row.tooltip(move |window, cx| {
                                Tooltip::new(path.display().to_string()).build(window, cx)
                            })
                        })
                        .when(hidden, |row| {
                            row.tooltip(|window, cx| {
                                Tooltip::new(super::sidebar::HIDDEN_BY_FILTER).build(window, cx)
                            })
                        });
                    // A drag that starts while the database is not known
                    // carries nothing, as the menus offer nothing then.
                    let dragged = match (shortcut, &query, leaf) {
                        _ if rename.is_some() => None,
                        (Some(shortcut), _, _) => Some(shortcut.pin()),
                        (None, Some(name), _) => Some(Pin::Member(Member::Query(name.clone()))),
                        (None, None, Some(leaf)) => workspace
                            .upgrade()
                            .and_then(|workspace| {
                                workspace.read(cx).member_in(&profile_id, leaf.target)
                            })
                            .map(Pin::Member),
                        _ => None,
                    };
                    let row = match dragged {
                        Some(pin) => {
                            let workspace = workspace.clone();
                            let dragged = DragShortcut {
                                profile: profile_id.clone(),
                                pin,
                                from: shortcut.and_then(|shortcut| shortcut.container(id)),
                                label: drag_label,
                            };
                            row.on_drag(dragged, move |dragged, grab, _, cx| {
                                _ = workspace.update(cx, |workspace, _| {
                                    workspace.explorer_drop = None;
                                });
                                cx.new(|_| DragLabel {
                                    label: dragged.label.clone(),
                                    grab,
                                })
                            })
                        }
                        None => row,
                    };
                    let row = match rename.is_none() && (pinned_group || shortcut.is_some()) {
                        true => drop_target(
                            row,
                            entry.item().id.clone(),
                            shortcut.cloned(),
                            &workspace,
                            t,
                        ),
                        false => row,
                    };
                    // A click inside the field is the field's.
                    if rename.is_some() || missing {
                        return row;
                    }
                    if let Some(path) = file {
                        let workspace = workspace.clone();
                        return row.on_click(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_listed_file(path.clone(), window, cx);
                            });
                        });
                    }
                    if let Some(name) = query {
                        let workspace = workspace.clone();
                        return row.on_click(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_saved_query(name.clone(), window, cx);
                            });
                        });
                    }
                    if let Some((object, kind)) = hidden_relation {
                        let workspace = workspace.clone();
                        return row.on_click(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_hidden_relation(&object, kind, window, cx);
                            });
                        });
                    }
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
            .context_menu(move |_, entry, menu, window, cx| {
                let id = entry.item().id.clone();
                let shortcut = menu_shortcuts.get(id.as_ref()).cloned();
                let Some(entity) = menu_workspace.upgrade() else {
                    return menu;
                };
                let state = entity.read(cx);
                if id.as_ref() == FILES_GROUP {
                    return files_group_menu(menu, state.settings.files_order, &menu_workspace);
                }
                let shortcuts = state
                    .profile()
                    .map(|profile| profile.shortcuts.clone())
                    .unwrap_or_default();
                if let Some(path) = menu_files.paths.get(id.as_ref()) {
                    return file_row_menu(menu, path, &menu_profile, &menu_workspace);
                }
                if is_shortcut_row(&id, shortcut.as_ref()) {
                    return shortcut_row_menu(
                        menu,
                        id,
                        shortcut.as_ref(),
                        &shortcuts,
                        &menu_profile,
                        &menu_workspace,
                        true,
                        window,
                        cx,
                    );
                }
                explorer_menu(
                    menu,
                    MenuRow {
                        leaf: menu_leaves.get(id.as_ref()).copied(),
                        label: entry.item().label.clone(),
                        id,
                        profile: menu_profile.clone(),
                        database: None,
                        listed: menu_database.clone(),
                        shortcut,
                    },
                    &menu_workspace,
                    window,
                    cx,
                )
            })
        });
        let content = div()
            .size_full()
            .flex()
            .flex_col()
            .children(message.map(|(message, color)| {
                div()
                    .p(px(layout::SPACE_MD))
                    .text_color(color)
                    .child(message)
            }))
            .children(tree.map(|tree| {
                div()
                    .flex_1()
                    .min_h_0()
                    .key_context("Explorer")
                    .track_focus(&self.explorer_focus)
                    .on_action(cx.listener(Self::open_sidebar_item))
                    .child(tree)
            }));

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
                    )
                    .when(lists_schemas, |row| {
                        row.child(self.render_fold_schemas(true, t))
                            .child(self.render_fold_schemas(false, t))
                    }),
            )
            .child(div().flex_1().min_h_0().child(content))
    }

    /// Dispatches what a bound shortcut does, so the two cannot come apart.
    fn render_fold_schemas(&self, open: bool, t: Theme) -> Button {
        let (id, glyph, label, chord) = match open {
            true => (
                "expand-schemas",
                icon::EXPAND_ALL,
                "Expand all schemas",
                "expand_all_schemas",
            ),
            false => (
                "collapse-schemas",
                icon::COLLAPSE_ALL,
                "Collapse all schemas",
                "collapse_all_schemas",
            ),
        };
        let hint = ui::chord_hint(chord, &self.settings.custom_keybindings);
        let tooltip = match hint.is_empty() {
            true => label.to_string(),
            false => format!("{label}  {hint}"),
        };
        icon_button(id, glyph, Tone::Quiet, Control::Inline, t)
            .tooltip(tooltip)
            .on_click(move |_, window, cx| match open {
                true => window.dispatch_action(Box::new(ExpandAllSchemas), cx),
                false => window.dispatch_action(Box::new(CollapseAllSchemas), cx),
            })
    }
}

/// One object's or schema's row of either tree, as its menu needs to know it.
pub(super) struct MenuRow {
    /// The row's id among simple mode's rows.
    pub(super) id: gpui::SharedString,
    pub(super) label: gpui::SharedString,
    /// The connection the row belongs to, which comes to the front before an
    /// item acts.
    pub(super) profile: String,
    /// The database the connections tree lists the row under.
    pub(super) database: Option<String>,
    /// The database the row was drawn from, which an export of it checks the
    /// connection is still on: `database` in the connections tree, and in
    /// simple mode, whose rows name none, the one its catalog is of.
    pub(super) listed: Option<String>,
    /// What the row opens, if it is an object.
    pub(super) leaf: Option<ExplorerLeaf>,
    pub(super) shortcut: Option<Shortcut>,
}

/// What an object's or a schema's row offers on a right click, in either
/// tree. Resolved against the row's own connection and database rather than
/// the one in front: a row's indices mean what they do in the catalog that
/// listed it, and a `main.t` of another database is not this one's.
pub(super) fn explorer_menu(
    menu: PopupMenu,
    row: MenuRow,
    workspace: &gpui::WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let Some(entity) = workspace.upgrade() else {
        return menu;
    };
    let state = entity.read(cx);
    let Some(profile) = state.profiles.iter().find(|found| found.id == row.profile) else {
        return menu;
    };
    let target = profile.config.target();
    let shortcuts = profile.shortcuts.clone();
    let held_in = match &row.shortcut {
        Some(Shortcut::Member { folder, .. }) => folder.clone(),
        _ => None,
    };
    // The connection's own database is the connection itself: it takes the
    // shortcuts, the export and the schema switch, which name no database.
    // What the menu offers is decided from it; an action is handed the row's
    // database as it is, and asks which that is when it runs, as a database
    // switch can land while the menu is open.
    let elsewhere = row
        .database
        .clone()
        .filter(|database| profile.active_database().as_deref() != Some(database.as_str()));
    // A diagram is of the row's database, read on a session of its own there.
    let draws_diagrams = profile.draws_diagrams();
    let node = state.row_node(&row.profile, row.database.as_deref(), &row.id, row.leaf);
    let Some(leaf) = row.leaf else {
        let Some(node) = node else {
            return menu;
        };
        let mut menu = menu;
        // First, where it was before the node actions came.
        if draws_diagrams && is_schema_row(&row.id) {
            let (workspace, profile_id) = (workspace.clone(), row.profile.clone());
            let (schema, database) = (row.label.to_string(), row.database.clone());
            menu = menu.item(
                PopupMenuItem::new("Show Diagram").on_click(move |_, window, cx| {
                    let (schema, database) = (schema.clone(), database.clone());
                    on_profile(&workspace, &profile_id, cx, |workspace, cx| {
                        workspace.open_diagram(database, schema, None, window, cx);
                    });
                }),
            );
        }
        if target.picks_schema() && elsewhere.is_none() {
            let (workspace, profile_id) = (workspace.clone(), row.profile.clone());
            let schema = row.label.to_string();
            menu = menu.item(
                PopupMenuItem::new("Set as Active Schema")
                    .checked(
                        profile
                            .active_schema()
                            .map(gpui::SharedString::from)
                            .as_ref()
                            == Some(&row.label),
                    )
                    .on_click(move |_, _, cx| {
                        let schema = schema.clone();
                        on_profile(&workspace, &profile_id, cx, |workspace, cx| {
                            workspace.set_schema(schema, cx);
                        });
                    }),
            );
        }
        let mut actions = node_actions(node.kind, &node.name, target);
        actions.retain(|action| {
            *action != NodeAction::FilterTables || state.table_scope_of(&node).is_some()
        });
        let menu = node_items(menu, &node, actions, workspace);
        // Last, below the node's own actions. The export reads the
        // connection's own database, so another one's schemas offer none.
        if !is_schema_row(&row.id) || elsewhere.is_some() || target.exports_schema().is_none() {
            return menu;
        }
        let (workspace, profile_id) = (workspace.clone(), row.profile.clone());
        let (schema, database) = (row.label.to_string(), row.listed.clone());
        return menu
            .separator()
            .item(
                PopupMenuItem::new("Export Schema as SQL…").on_click(move |_, _, cx| {
                    let (schema, database) = (schema.clone(), database.clone());
                    on_profile(&workspace, &profile_id, cx, |workspace, cx| {
                        workspace.open_schema_export(database, schema, cx);
                    });
                }),
            );
    };
    let member = match elsewhere {
        None => state.member_in(&row.profile, leaf.target),
        Some(_) => None,
    };
    let unknown = elsewhere.is_none() && profile.shortcut_database() == ShortcutDatabase::Unknown;
    let with_shortcuts = |menu: PopupMenu, window: &mut Window, cx: &mut App| match member.clone() {
        Some(member) => shortcut_menu(
            menu,
            member,
            held_in.clone(),
            &shortcuts,
            &row.profile,
            workspace,
            window,
            cx,
        ),
        None if unknown => unknown_database_items(menu),
        None => menu,
    };
    let Some(node) = node else {
        return with_shortcuts(menu, window, cx);
    };
    let (schema, relation, kind) = match (node.kind, &node.schema) {
        (NodeKind::Relation(kind), Some(schema)) => (schema.clone(), node.name.clone(), kind),
        // A routine or a sequence offers its own actions and what a folder or
        // a pin does, and no more.
        _ => {
            let actions = node_actions(node.kind, &node.name, target);
            let menu = node_items(menu, &node, actions, workspace);
            return with_shortcuts(menu, window, cx);
        }
    };
    let engine = target.engine;
    let copy = |label: &'static str, text: String| {
        PopupMenuItem::new(label).on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        })
    };
    let ddl_names = (schema.clone(), relation.clone());
    let open = |label: &'static str, structure: bool| {
        let (workspace, profile) = (workspace.clone(), row.profile.clone());
        let (schema, relation, database) = (schema.clone(), relation.clone(), row.database.clone());
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            let (schema, relation, database) = (schema.clone(), relation.clone(), database.clone());
            on_profile(&workspace, &profile, cx, |workspace, cx| {
                workspace
                    .open_relation_view(database, schema, relation, kind, structure, window, cx);
            });
        })
    };
    let import_names = (schema.clone(), relation.clone());
    // Built here rather than through `PopupMenu::submenu`, for the reason the
    // grid's "Copy Rows As" gives: that wants the menu's own context, and a
    // tree hands over its own.
    let (export_workspace, export_names, export_profile) = (
        workspace.clone(),
        (row.listed.clone(), schema.clone(), relation.clone()),
        row.profile.clone(),
    );
    let exports = PopupMenu::build(window, cx, move |submenu, _, _| {
        let export = |label: &'static str, format: FileAs| {
            let (workspace, profile) = (export_workspace.clone(), export_profile.clone());
            let (database, schema, relation) = export_names.clone();
            PopupMenuItem::new(label).on_click(move |_, _, cx| {
                let (database, schema, relation) =
                    (database.clone(), schema.clone(), relation.clone());
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    workspace.export_relation(format, database, schema, relation, cx);
                });
            })
        };
        let submenu = submenu
            .item(export("CSV", FileAs::Rows(Format::Csv)))
            .item(export("JSON", FileAs::Rows(Format::Json)));
        match engine.syntax() {
            db::Syntax::Sql => submenu.item(export("SQL", FileAs::Inserts)),
            _ => submenu,
        }
    });
    let mut menu = menu
        .item(open("Open Data", false))
        .item(open("Open Structure", true));
    if draws_diagrams && super::drawn_in_diagrams(kind) {
        let (workspace, profile) = (workspace.clone(), row.profile.clone());
        let (schema, relation, database) = (schema.clone(), relation.clone(), row.database.clone());
        menu = menu.item(
            PopupMenuItem::new("Show in Diagram").on_click(move |_, window, cx| {
                let (schema, relation, database) =
                    (schema.clone(), relation.clone(), database.clone());
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    workspace.open_diagram(database, schema, Some(relation), window, cx);
                });
            }),
        );
    }
    if target.takes_imports(kind) {
        let (workspace, profile, database) =
            (workspace.clone(), row.profile.clone(), row.database.clone());
        menu = menu.item(
            PopupMenuItem::new("Import Rows…").on_click(move |_, window, cx| {
                let (schema, relation) = import_names.clone();
                let database = database.clone();
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    workspace.import_rows_into(database, schema, relation, window, cx);
                });
            }),
        );
    }
    // An export names no database, so only the connection's own are offered.
    if elsewhere.is_none() {
        menu = menu.item(PopupMenuItem::submenu("Export", exports));
    }
    let menu = relation_copies(target, &schema, &relation, kind)
        .into_iter()
        .fold(menu, |menu, group| {
            group
                .into_iter()
                .fold(menu.separator(), |menu, (label, text)| match text {
                    Some(text) => menu.item(copy(label, text)),
                    None => {
                        let (workspace, profile, database, names) = (
                            workspace.clone(),
                            row.profile.clone(),
                            row.database.clone(),
                            ddl_names.clone(),
                        );
                        let (show_workspace, show_node) = (workspace.clone(), node.clone());
                        let (grants_workspace, grants_node) = (workspace.clone(), node.clone());
                        menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                            let (schema, relation) = names.clone();
                            let database = database.clone();
                            on_profile(&workspace, &profile, cx, |workspace, cx| {
                                workspace.copy_ddl(database, schema, relation, kind, cx);
                            });
                        }))
                        .item(PopupMenuItem::new(NodeAction::ShowDdl.label()).on_click(
                            move |_, window, cx| {
                                let node = show_node.clone();
                                _ = show_workspace.update(cx, |workspace, cx| {
                                    workspace.run_node_action(
                                        &node,
                                        NodeAction::ShowDdl,
                                        window,
                                        cx,
                                    )
                                });
                            },
                        ))
                        .when(target.lists_roles(), |menu| {
                            menu.item(PopupMenuItem::new(NodeAction::ShowGrants.label()).on_click(
                                move |_, window, cx| {
                                    let node = grants_node.clone();
                                    _ = grants_workspace.update(cx, |workspace, cx| {
                                        workspace.run_node_action(
                                            &node,
                                            NodeAction::ShowGrants,
                                            window,
                                            cx,
                                        )
                                    });
                                },
                            ))
                        })
                    }
                })
        });
    with_shortcuts(menu, window, cx)
}

/// Whether `id` is a row [`shortcut_row_menu`] draws the menu of: a saved
/// query, a folder, the Pinned group, or a pin whose object is gone or the
/// connection's filter hides.
pub(super) fn is_shortcut_row(id: &str, shortcut: Option<&Shortcut>) -> bool {
    matches!(
        shortcut,
        Some(
            Shortcut::Folder(_)
                | Shortcut::Member { missing: true, .. }
                | Shortcut::Member { hidden: true, .. }
        )
    ) || id == PINNED_GROUP
        || saved_query(id)
            .or_else(|| shortcut.and_then(Shortcut::query))
            .is_some()
}

/// The menu of a row [`is_shortcut_row`] says is one, in either tree, acting
/// on `profile`. `renames` is whether the tree draws the field a Rename…
/// types into, which the connections tree does not.
// The row, what it is among the shortcuts, the shortcuts, the connection and
// the workspace are each a different thing, as for `shortcut_menu`.
#[allow(clippy::too_many_arguments)]
pub(super) fn shortcut_row_menu(
    menu: PopupMenu,
    id: gpui::SharedString,
    shortcut: Option<&Shortcut>,
    shortcuts: &Shortcuts,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
    renames: bool,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let held_in = match shortcut {
        Some(Shortcut::Member { folder, .. }) => folder.clone(),
        _ => None,
    };
    match shortcut {
        Some(Shortcut::Folder(name)) => {
            let pinned = shortcuts.pinned(&Pin::Folder(name.clone()));
            return folder_menu(menu, name, id, pinned, profile, workspace, renames);
        }
        Some(Shortcut::Member {
            member,
            missing: true,
            ..
        })
        | Some(Shortcut::Member {
            member,
            hidden: true,
            ..
        }) => {
            return removal_items(menu, member, held_in, shortcuts, profile, workspace);
        }
        _ => {}
    }
    if id.as_ref() == PINNED_GROUP {
        let (workspace, profile) = (workspace.clone(), profile.to_string());
        return menu.item(
            PopupMenuItem::new("New Folder…").on_click(move |_, window, cx| {
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    workspace.new_folder(None, window, cx);
                });
            }),
        );
    }
    let query = saved_query(&id)
        .or_else(|| shortcut.and_then(Shortcut::query))
        .map(str::to_string);
    let Some(name) = query else {
        return menu;
    };
    let menu = saved_query_menu(menu, &name, id, profile, workspace, renames);
    shortcut_menu(
        menu,
        Member::Query(name),
        held_in,
        shortcuts,
        profile,
        workspace,
        window,
        cx,
    )
}

/// What the Files group's rows of `profile` stand for, by row id, read once
/// per frame for the tree's row closure.
#[derive(Clone, Default)]
pub(super) struct ListedFiles {
    pub(super) paths: HashMap<String, PathBuf>,
    pub(super) open: HashSet<String>,
    /// Gone at the last look, and not open in a tab, which keeps its text.
    pub(super) missing: HashSet<String>,
}

impl ListedFiles {
    pub(super) fn of(profile: &Profile) -> Self {
        let open: HashSet<String> = profile
            .session
            .queries
            .iter()
            .filter_map(|tab| Some(explorer::file_row(&tab.file.as_ref()?.path)))
            .collect();
        let missing = profile
            .missing_files
            .iter()
            .map(|path| explorer::file_row(path))
            .filter(|row| !open.contains(row))
            .collect();
        Self {
            paths: profile
                .listed_files()
                .into_iter()
                .map(|path| (explorer::file_row(&path), path))
                .collect(),
            open,
            missing,
        }
    }
}

/// What revealing a file is called where it is revealed.
#[cfg(target_os = "macos")]
const REVEAL: &str = "Reveal in Finder";
#[cfg(target_os = "windows")]
const REVEAL: &str = "Reveal in File Explorer";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const REVEAL: &str = "Open Containing Folder";

/// What a listed file's row offers, in either tree. Remove from List leaves
/// the file where it is: deleting one is the file manager's job.
pub(super) fn file_row_menu(
    menu: PopupMenu,
    path: &Path,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenu {
    let item =
        |label: &'static str,
         act: fn(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>)| {
            let (workspace, profile, path) =
                (workspace.clone(), profile.to_string(), path.to_path_buf());
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    act(workspace, path.clone(), window, cx)
                });
            })
        };
    menu.item(item("Open", |workspace, path, window, cx| {
        workspace.open_listed_file(path, window, cx)
    }))
    .item(item(REVEAL, |_, path, _, cx| cx.reveal_path(&path)))
    .separator()
    .item(item("Remove from List", |workspace, path, _, cx| {
        workspace.forget_recent_file(&path, cx)
    }))
}

/// What the Files group's own row offers, in either tree: the order of its
/// rows, which is every connection's.
pub(super) fn files_group_menu(
    menu: PopupMenu,
    current: FilesOrder,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenu {
    FilesOrder::ALL.into_iter().fold(menu, |menu, order| {
        let workspace = workspace.clone();
        menu.item(
            PopupMenuItem::new(order.label())
                .checked(order == current)
                .on_click(move |_, _, cx| {
                    _ = workspace.update(cx, |workspace, cx| workspace.set_files_order(order, cx));
                }),
        )
    })
}

/// What a saved query's row offers on a right click: what its tab offers, for
/// a query that may have no tab open.
fn saved_query_menu(
    menu: PopupMenu,
    name: &str,
    row: gpui::SharedString,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
    renames: bool,
) -> PopupMenu {
    let action =
        |label: &'static str,
         act: fn(&mut Workspace, String, &mut Window, &mut Context<Workspace>)| {
            let (workspace, profile) = (workspace.clone(), profile.to_string());
            let name = name.to_string();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                on_profile(&workspace, &profile, cx, |workspace, cx| {
                    act(workspace, name.clone(), window, cx)
                });
            })
        };
    let menu = menu.item(action("Open", |workspace, name, window, cx| {
        workspace.open_saved_query(name, window, cx)
    }));
    let menu = match renames {
        true => menu.item({
            let workspace = workspace.clone();
            let name = name.to_string();
            PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.start_query_rename(name.clone(), row.clone(), window, cx)
                });
            })
        }),
        false => menu,
    };
    menu.separator()
        .item(action("Delete…", |workspace, name, _, cx| {
            workspace.ask_delete_saved_query(name, cx)
        }))
}

/// What a folder's row offers.
fn folder_menu(
    menu: PopupMenu,
    name: &str,
    row: gpui::SharedString,
    pinned: bool,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
    renames: bool,
) -> PopupMenu {
    let menu = match renames {
        true => {
            let (workspace, name) = (workspace.clone(), name.to_string());
            menu.item(
                PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.start_rename(name.clone(), true, row.clone(), window, cx)
                    });
                }),
            )
        }
        false => menu,
    };
    let (delete, folder) = ((workspace.clone(), profile.to_string()), name.to_string());
    menu.item(pin_item(
        pinned,
        Pin::Folder(name.to_string()),
        profile,
        workspace,
    ))
    .separator()
    .item(PopupMenuItem::new("Delete…").on_click(move |_, _, cx| {
        let (workspace, profile) = &delete;
        on_profile(workspace, profile, cx, |workspace, cx| {
            workspace.ask_delete_folder(folder.clone(), cx)
        });
    }))
}

/// What every object's and saved query's row offers for folders and pins,
/// wherever the row is drawn: "Add to Folder" with a check on each folder it
/// is in, which toggles, and Pin or Unpin.
// The member's identity, where it is held, the shortcuts it joins and the
// connection and workspace an item acts on are each a different thing, and a
// struct for them would be a type its two callers fill and nothing else reads.
#[allow(clippy::too_many_arguments)]
fn shortcut_menu(
    menu: PopupMenu,
    member: Member,
    held_in: Option<String>,
    shortcuts: &Shortcuts,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let mut folders: Vec<(String, bool)> = shortcuts
        .folders
        .iter()
        .map(|folder| (folder.name.clone(), shortcuts.holds(&folder.name, &member)))
        .collect();
    folders.sort_by_cached_key(|(name, _)| name.to_lowercase());
    let (submenu_workspace, submenu_member) = (workspace.clone(), member.clone());
    let submenu_profile = profile.to_string();
    // Built here rather than through `PopupMenu::submenu`, as "Export" is.
    let submenu = PopupMenu::build(window, cx, move |submenu, _, _| {
        let toggle = |name: &String, held: bool| {
            let (workspace, member, name, profile) = (
                submenu_workspace.clone(),
                submenu_member.clone(),
                name.clone(),
                submenu_profile.clone(),
            );
            PopupMenuItem::new(name.clone())
                .checked(held)
                .on_click(move |_, _, cx| {
                    on_profile(&workspace, &profile, cx, |workspace, cx| {
                        workspace.toggle_in_folder(name.clone(), member.clone(), cx)
                    });
                })
        };
        let submenu = folders.iter().fold(submenu, |submenu, (name, held)| {
            submenu.item(toggle(name, *held))
        });
        let submenu = match folders.is_empty() {
            true => submenu,
            false => submenu.separator(),
        };
        submenu.item(
            PopupMenuItem::new("New Folder…").on_click(move |_, window, cx| {
                _ = submenu_workspace.update(cx, |workspace, cx| {
                    workspace.make_front(&submenu_profile, cx);
                    workspace.new_folder(Some(submenu_member.clone()), window, cx)
                });
            }),
        )
    });
    let pin = Pin::Member(member.clone());
    let menu = menu
        .separator()
        .item(PopupMenuItem::submenu("Add to Folder", submenu))
        .item(pin_item(shortcuts.pinned(&pin), pin, profile, workspace));
    match held_in {
        Some(folder) => menu.item(remove_item(folder, member, profile, workspace)),
        None => menu,
    }
}

/// Folders and pins, greyed while the database an object is in is not known:
/// a shortcut made without it would open the same name in any database.
fn unknown_database_items(menu: PopupMenu) -> PopupMenu {
    menu.separator()
        .item(PopupMenuItem::new("Add to Folder").disabled(true))
        .item(PopupMenuItem::new("Pin").disabled(true))
        .label("The database is not known yet.")
}

/// All a missing member's row offers: taking it out of where it is listed.
fn removal_items(
    menu: PopupMenu,
    member: &Member,
    held_in: Option<String>,
    shortcuts: &Shortcuts,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenu {
    let pin = Pin::Member(member.clone());
    let menu = match held_in {
        Some(folder) => menu.item(remove_item(folder, member.clone(), profile, workspace)),
        None => menu,
    };
    match shortcuts.pinned(&pin) {
        true => menu.item(pin_item(true, pin, profile, workspace)),
        false => menu,
    }
}

/// A row in a folder or the Pinned group, or the group itself, taking a
/// dragged shortcut: a folder's row and the group's light up, and a member's
/// draws the line between its neighbours where the drop would go.
fn drop_target(
    row: ListItem,
    id: gpui::SharedString,
    shortcut: Option<Shortcut>,
    workspace: &gpui::WeakEntity<Workspace>,
    t: Theme,
) -> ListItem {
    let land = {
        let id = id.clone();
        std::rc::Rc::new(move |dragged: &Pin, after: bool| {
            landing(&id, shortcut.as_ref(), dragged, after)
        })
    };
    let (moved, dropped) = (workspace.clone(), workspace.clone());
    row.can_drop({
        let land = land.clone();
        move |dragged, _, _| {
            dragged
                .downcast_ref::<DragShortcut>()
                .is_some_and(|dragged| land(&dragged.pin, false).is_some())
        }
    })
    .drag_over::<DragShortcut>({
        let land = land.clone();
        move |style, dragged, _, _| match land(&dragged.pin, false) {
            Some(Landing::Folder(_) | Landing::Pinned) => style.bg(t.element_active),
            _ => style,
        }
    })
    // Every row hears every move, so each says only whether it is the one
    // under the pointer.
    .on_drag_move::<DragShortcut>({
        let (land, row) = (land.clone(), id.clone());
        move |event, _, cx| {
            let (position, bounds) = (event.event.position, event.bounds);
            let after = position.y > bounds.center().y;
            let beside = bounds.contains(&position)
                && matches!(
                    land(&event.drag(cx).pin, after),
                    Some(Landing::Beside { .. })
                );
            _ = moved.update(cx, |workspace, cx| {
                workspace.hover_drop(&row, beside.then_some(after), cx)
            });
        }
    })
    .on_drop(move |dragged: &DragShortcut, _, cx| {
        _ = dropped.update(cx, |workspace, cx| {
            let after = workspace
                .explorer_drop
                .as_ref()
                .is_some_and(|(row, after)| *row == id && *after);
            if let Some(landing) = land(&dragged.pin, after) {
                workspace.drop_shortcut(dragged, landing, cx);
            }
        });
    })
}

fn pin_item(
    pinned: bool,
    pin: Pin,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenuItem {
    let (workspace, profile) = (workspace.clone(), profile.to_string());
    PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" }).on_click(move |_, _, cx| {
        on_profile(&workspace, &profile, cx, |workspace, cx| {
            workspace.toggle_pin(pin.clone(), cx)
        });
    })
}

fn remove_item(
    folder: String,
    member: Member,
    profile: &str,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenuItem {
    let (workspace, profile) = (workspace.clone(), profile.to_string());
    PopupMenuItem::new("Remove from Folder").on_click(move |_, _, cx| {
        on_profile(&workspace, &profile, cx, |workspace, cx| {
            workspace.toggle_in_folder(folder.clone(), member.clone(), cx)
        });
    })
}

/// The items a node's `actions` make, each running through
/// [`Workspace::run_node_action`] on the node the menu was built for.
pub(super) fn node_items(
    menu: PopupMenu,
    node: &SelectedNode,
    actions: Vec<NodeAction>,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenu {
    actions.into_iter().fold(menu, |menu, action| {
        let (workspace, node) = (workspace.clone(), node.clone());
        menu.item(PopupMenuItem::new(action.label()).on_click({
            let action = action.clone();
            move |_, window, cx| {
                let (node, action) = (node.clone(), action.clone());
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.run_node_action(&node, action, window, cx)
                });
            }
        }))
    })
}

/// The menu of a selection of several rows: how many, then what applies to
/// all of them.
pub(super) fn selection_menu(
    menu: PopupMenu,
    header: String,
    actions: Vec<SelectionAction>,
    workspace: &gpui::WeakEntity<Workspace>,
) -> PopupMenu {
    actions.into_iter().fold(
        menu.item(PopupMenuItem::label(header)).separator(),
        |menu, action| {
            let workspace = workspace.clone();
            menu.item(
                PopupMenuItem::new(action.label()).on_click(move |_, window, cx| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.run_selection_action(action, window, cx)
                    });
                }),
            )
        },
    )
}

/// Run `act` with the connection `profile` in front. A row of another
/// connection in the connections tree acts for that connection: the gate, the
/// mode and the shortcuts an item meets are its own, never the front one's.
fn on_profile(
    workspace: &gpui::WeakEntity<Workspace>,
    profile: &str,
    cx: &mut App,
    act: impl FnOnce(&mut Workspace, &mut Context<Workspace>),
) {
    _ = workspace.update(cx, |workspace, cx| {
        workspace.make_front(profile, cx);
        act(workspace, cx)
    });
}

/// A saved query's name as a field, laid over the label it replaces. The
/// label stays, unseen, to hold the row's height: the tree's rows are all as
/// tall as its first, and a field taller than a label would push nothing but
/// spill over its neighbours. A refusal floats under the field for the same
/// reason.
fn rename_field(
    label: StyledText,
    input: &Entity<InputState>,
    error: Option<String>,
    t: Theme,
) -> gpui::Div {
    let field = div()
        .relative()
        .w_full()
        .h(px(layout::chrome(layout::CONTROL_HEIGHT_INLINE)))
        .flex()
        .items_center()
        .rounded(px(layout::RADIUS_CONTROL))
        .border_1()
        .border_color(if error.is_some() { t.danger } else { t.accent })
        .bg(t.bg)
        .child(
            Input::new(input)
                .appearance(false)
                .h_full()
                .px(px(layout::SPACE_XS))
                .py_0()
                .text_size(px(layout::chrome(layout::TEXT_MD))),
        )
        .children(error.map(|message| {
            div()
                .absolute()
                .top_full()
                .left_0()
                .mt(px(2.))
                .child(gpui::deferred(
                    gpui::anchored()
                        .snap_to_window_with_margin(px(layout::SPACE_SM))
                        .child(raised_card(
                            div()
                                .max_w(px(layout::SIDEBAR_DEFAULT_WIDTH))
                                .px(px(layout::SPACE_SM))
                                .py(px(layout::SPACE_XS))
                                .rounded(px(layout::RADIUS_CONTROL))
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.danger)
                                .child(message),
                            t,
                        )),
                ))
        }));
    div()
        .flex_1()
        .min_w_0()
        .relative()
        .child(div().invisible().whitespace_nowrap().child(label))
        .child(
            // Out by the field's own inset, so the name does not move as the
            // row turns into it.
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-(layout::SPACE_XS + 1.)))
                .right_0()
                .flex()
                .items_center()
                .child(field),
        )
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

/// A field of the connection form: its label, then whatever takes the rest of
/// the row. The label focuses the field, as a form's label does.
fn field_row(
    id: &'static str,
    label: &'static str,
    input: &Entity<InputState>,
    control: impl IntoElement,
    t: Theme,
) -> gpui::Div {
    nested_field_row(0., id, label, input, control, t)
}

/// [`field_row`] with its label stepped in by `indent`, for a field that
/// belongs to the switch above it. The field itself stays in line with the
/// others.
fn nested_field_row(
    indent: f32,
    id: &'static str,
    label: &'static str,
    input: &Entity<InputState>,
    control: impl IntoElement,
    t: Theme,
) -> gpui::Div {
    let target = input.clone();
    div()
        .flex()
        .items_center()
        .min_h(px(layout::chrome(layout::FORM_ROW_HEIGHT)))
        .px(px(layout::SPACE_LG))
        .child(
            div()
                .id(id)
                .flex_shrink_0()
                .w(px(layout::FORM_LABEL_WIDTH))
                .pl(px(indent))
                .text_color(t.text_muted)
                .child(label)
                .on_click(move |_, window, cx| target.focus_handle(cx).focus(window, cx)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .child(control),
        )
}

/// A small boxed field for a number of seconds, its unit after it.
fn seconds_field(input: &Entity<InputState>, t: Theme) -> gpui::Div {
    number_field(input, "s", t)
}

/// A small boxed field for a whole number of `unit`, the unit after it.
fn number_field(input: &Entity<InputState>, unit: &'static str, t: Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .w(px(layout::chrome(layout::FORM_TIMEOUT_WIDTH)))
        .h(px(Control::Compact.height()))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .border_1()
        .border_color(t.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(gpui_component::Sizable::small(bare(input))),
        )
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_faint)
                .child(unit),
        )
}

/// A field with nothing around it: the row it sits in is the box.
fn bare(input: &Entity<InputState>) -> Input {
    Input::new(input).appearance(false).p_0()
}

/// The path fields the form is drawing, each with a [`browse_button`].
fn shown_file_fields<'a>(form: &'a ConnectionForm, cx: &App) -> Vec<&'a Entity<InputState>> {
    let fields = form.target().fields();
    match fields {
        Fields::File => vec![&form.path],
        Fields::Account => vec![&form.private_key],
        Fields::Server | Fields::ConnectionString => {
            let mut shown = Vec::new();
            if fields == Fields::Server && form.shows_root_certificate(cx) {
                shown.push(&form.root_certificate);
            }
            if form.ssh {
                shown.push(&form.ssh_identity_file);
            }
            if form.ssm && form.ssm_advanced {
                shown.extend([&form.ssm_aws_cli, &form.ssm_plugin]);
            }
            shown
        }
    }
}

fn browse_button(
    id: &'static str,
    input: &Entity<InputState>,
    t: Theme,
    cx: &Context<Workspace>,
) -> gpui_component::button::Button {
    let input = input.clone();
    icon_button(id, icon::BROWSE, Tone::Quiet, Control::Compact, t)
        .ml(px(layout::SPACE_SM))
        .tooltip("Choose a file")
        .on_click(cx.listener(move |workspace, _, window, cx| {
            workspace.browse_into(input.clone(), window, cx);
        }))
}

/// A port after its host, behind a hairline and the colon that joins the two
/// in a URL.
fn port_field(input: &Entity<InputState>, t: Theme) -> gpui::Div {
    div()
        .flex_shrink_0()
        .w(px(layout::FORM_PORT_WIDTH))
        .ml(px(layout::SPACE_SM))
        .pl(px(layout::SPACE_SM))
        .border_l_1()
        .border_color(t.border)
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .child(div().text_color(t.text_faint).child(":"))
        .child(div().flex_1().min_w_0().child(bare(input)))
}

const TUNNEL_REVEAL_MS: u64 = 220;
const TUNNEL_REVEAL_PX: f32 = 4.;

/// The tunnel rows, settling in once as the tunnel is switched on. Keyed by the
/// row, so it plays each time they mount and never again while they stay.
fn revealed(row: gpui::Div, section: &'static str, index: usize, animate: bool) -> AnyElement {
    if !animate {
        return row.into_any_element();
    }
    row.relative()
        .with_animation(
            (section, index),
            Animation::new(Duration::from_millis(TUNNEL_REVEAL_MS)).with_easing(ease_out_quint()),
            |row, delta| row.opacity(delta).top(px(-TUNNEL_REVEAL_PX * (1. - delta))),
        )
        .into_any_element()
}

/// Whether the sidebar draws its tree: with a catalog that lists something,
/// and while the catalog is away too, so a connection that is down or still
/// loading has its files, saved queries, folders and pins within reach.
pub(super) fn explorer_drawn(profile: &Profile) -> bool {
    matches!(&profile.catalog, CatalogState::Loaded(catalog, _) if !catalog.schemas.is_empty())
        || !profile.session.saved_queries.is_empty()
        || !profile.listed_files().is_empty()
        || profile.shortcuts != Shortcuts::default()
}

/// What the form's footer says beside Test: a Help command's notice while it
/// lasts, then the form's error, then the last test.
fn footer_status(
    notice: Option<&(Status, String)>,
    error: Option<&String>,
    test: Option<&ConnectionTest>,
) -> Option<(Status, String)> {
    if let Some((status, message)) = notice {
        return Some((*status, one_line(message)));
    }
    match (error, test) {
        (Some(error), _) => Some((Status::Failed, one_line(error))),
        (None, Some(ConnectionTest::Running(_, _))) => {
            Some((Status::Connecting, "Testing connection…".to_string()))
        }
        (None, Some(ConnectionTest::Passed(reported))) => Some((
            Status::Connected,
            reported
                .as_ref()
                .map_or_else(|| "Connection succeeded.".to_string(), Reported::sentence),
        )),
        (None, Some(ConnectionTest::Failed(message))) => Some((Status::Failed, one_line(message))),
        (None, None) => None,
    }
}

/// What a dialog button does when pressed.
type DialogAnswer = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// The button the ring starts on when `dialog` goes up, counted from the right:
/// Cancel for the one that ends the user's transaction and the one that widens
/// what an agent may reach, the primary for every other.
fn first_choice(dialog: Option<&str>) -> usize {
    match dialog {
        // Cancel, so an `enter` held or repeated from the palette row or click
        // that raised it cancels rather than grants.
        Some("agent-visibility" | "agent-writes-auto") => 1,
        // Commit and Roll back cannot be undone, so an `enter` held or
        // repeated from the click that raised it opens nothing it should not.
        Some("end-transaction") => 2,
        // Quitting rolls the transaction back.
        Some("quit") => 1,
        _ => 0,
    }
}

/// How a `[U+XXXX]` marker in the approval's statement is drawn: DBDelve's own,
/// on the accent so it cannot pass for the statement's text, in the ink every
/// other label on the accent wears.
fn approval_marker(t: Theme) -> gpui::HighlightStyle {
    gpui::HighlightStyle {
        color: Some(t.on_accent.into()),
        background_color: Some(t.accent.into()),
        ..Default::default()
    }
}

/// The ink of Approve's label: the faint one until it arms, so the one redraw
/// the arm causes is a change the person can see, and the quiet tone's once it
/// has. A change of state, never a countdown (D-09).
fn approve_ink(t: Theme, armed: bool) -> theme::color::Srgb {
    if armed {
        Tone::Quiet.ink(t)
    } else {
        t.text_faint
    }
}

/// Whether a click is one Approve answers: a single mouse click that is not the
/// one that brought the window forward. Refused, each for its own reason:
/// - a double click, whose second press may land on a button that appeared
///   under the pointer after the first;
/// - the activating click, which was aimed at the window and not at Approve;
/// - a key, which must never approve (the button is not a tab stop, and
///   `enter` on the dialog is Deny);
/// - a touch, which has no place on a desktop dialog.
fn approves(event: &gpui::ClickEvent) -> bool {
    matches!(event, gpui::ClickEvent::Mouse(_)) && event.click_count() == 1 && !event.first_focus()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_wears_the_ink_drawn_on_the_accent() {
        for theme in theme::Theme::built_in() {
            let t = theme.for_popup();
            let marker = approval_marker(t);
            assert_eq!(marker.color, Some(t.on_accent.into()));
            assert_eq!(marker.background_color, Some(t.accent.into()));
        }
    }

    #[test]
    fn approve_is_drawn_dimmer_until_it_arms_on_every_theme() {
        for theme in theme::Theme::built_in() {
            let t = theme.for_popup();
            assert_eq!(approve_ink(t, true), Tone::Quiet.ink(t));
            assert_ne!(approve_ink(t, false), approve_ink(t, true));
        }
    }

    #[test]
    fn approve_takes_only_a_single_ordinary_mouse_click() {
        use gpui::{ClickEvent, MouseClickEvent, MouseDownEvent, MouseUpEvent};

        let mouse = |count, first_mouse| {
            ClickEvent::Mouse(MouseClickEvent {
                down: MouseDownEvent {
                    click_count: count,
                    first_mouse,
                    ..Default::default()
                },
                up: MouseUpEvent {
                    click_count: count,
                    ..Default::default()
                },
            })
        };
        assert!(approves(&mouse(1, false)), "a plain click");
        assert!(!approves(&mouse(2, false)), "a double click");
        assert!(!approves(&mouse(1, true)), "the click that woke the window");
        assert!(
            !approves(&ClickEvent::Keyboard(Default::default())),
            "a key"
        );
        assert!(!approves(&ClickEvent::Touch(Default::default())), "a touch");
    }

    #[test]
    fn a_help_notice_covers_the_form_status_without_replacing_it() {
        let error = "password authentication failed".to_string();
        let notice = (Status::Idle, "Diagnostics copied.".to_string());
        assert_eq!(
            footer_status(Some(&notice), Some(&error), None),
            Some(notice.clone())
        );
        assert_eq!(
            footer_status(None, Some(&error), None),
            Some((Status::Failed, error))
        );
        let failed = ConnectionTest::Failed("timed out".into());
        assert_eq!(
            footer_status(Some(&notice), None, Some(&failed)),
            Some(notice)
        );
        assert_eq!(
            footer_status(None, None, Some(&failed)),
            Some((Status::Failed, "timed out".into()))
        );
    }
}
