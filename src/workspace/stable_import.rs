//! Import from DBDelve, offered by a nightly only: a stable install's
//! connections, saved passwords, queries and settings brought into this one,
//! then a restart to open them.
//!
//! A restart rather than a reload in place: the plugins, the user themes and
//! the keymap are all read once, at launch, before the workspace exists, and
//! an import can bring each of them.

use std::path::PathBuf;

use gpui_component::spinner::Spinner;
use gpui_component::{IconName, Sizable};

use super::*;
use crate::store::stable_import::{self, Listed, Services, Staged, Swapped, SystemKeychain};

/// Where an import is, as its dialog shows it.
pub(crate) enum StableImport {
    /// Asked first, since the data here is moved aside to `backup`.
    Confirm {
        backup: PathBuf,
    },
    Running,
    Done(Imported),
}

pub(crate) struct Imported {
    connections: usize,
    backup: Option<PathBuf>,
    /// The connections a saved password or key passphrase didn't come
    /// across for.
    without_secrets: Vec<String>,
}

/// How many connections a DBDelve install here has to import. None on stable
/// itself, which offers no import.
pub(crate) fn stable_connections() -> usize {
    if Channel::current() != Channel::Nightly {
        return 0;
    }
    stable_import::source().map_or(0, |source| stable_import::listed(&source).len())
}

impl Workspace {
    pub(crate) fn import_from_stable(&mut self, cx: &mut Context<Self>) {
        if self.stable_connections == 0 || self.stable_import.is_some() {
            return;
        }
        let target = match store::dbdelve_directory() {
            Ok(target) => target,
            Err(message) => return self.note(message, cx),
        };
        if stable_import::holds_data(&target) {
            self.stable_import = Some(StableImport::Confirm {
                backup: stable_import::backup_of(&target),
            });
            cx.notify();
        } else {
            self.run_stable_import(cx);
        }
    }

    pub(crate) fn cancel_stable_import(&mut self, cx: &mut Context<Self>) {
        if matches!(self.stable_import, Some(StableImport::Confirm { .. })) {
            self.stable_import = None;
            cx.notify();
        }
    }

    /// Copies in the background and swaps on this thread, where nothing else
    /// writes to the directory meanwhile. The saved passwords come last, off
    /// this thread: on macOS each can wait on a Keychain prompt.
    pub(crate) fn run_stable_import(&mut self, cx: &mut Context<Self>) {
        let directories =
            stable_import::source().and_then(|source| Ok((source, store::dbdelve_directory()?)));
        let (source, target) = match directories {
            Ok(directories) => directories,
            Err(message) => return self.stable_import_failed(message, cx),
        };
        self.stable_import = Some(StableImport::Running);
        cx.notify();
        let staged = spawn_blocking(cx.background_executor(), move || {
            stable_import::stage(&source, &target)
        });
        cx.spawn(async move |workspace, cx| {
            let staged = staged.await;
            let Ok(Some((connections, swapped))) =
                workspace.update(cx, |workspace, cx| workspace.swap_in_import(staged, cx))
            else {
                return;
            };
            let count = connections.len();
            let Swapped { backup, discarded } = swapped;
            let without_secrets = spawn_blocking(cx.background_executor(), move || {
                if let Some(discarded) = discarded {
                    let _ = std::fs::remove_dir_all(discarded);
                }
                copy_saved_passwords(&connections)
            });
            let without_secrets = without_secrets.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.stable_import = Some(StableImport::Done(Imported {
                    connections: count,
                    backup,
                    without_secrets,
                }));
                cx.notify();
            });
        })
        .detach();
    }

    /// The connections the import lists, and where the swap put what was
    /// here.
    fn swap_in_import(
        &mut self,
        staged: Result<Staged, String>,
        cx: &mut Context<Self>,
    ) -> Option<(Vec<Listed>, Swapped)> {
        let staged = match staged {
            Ok(staged) => staged,
            Err(message) => {
                self.stable_import_failed(message, cx);
                return None;
            }
        };
        // What this session holds reaches the disk first, so the copy kept
        // aside is the data as it is now, as a quit would have left it.
        self.persist_buffers(cx);
        if self.profiles_unsaved {
            let _ = self.save_profiles(cx);
        }
        let connections = staged.connections.clone();
        match stable_import::swap(staged) {
            Ok(swapped) => {
                self.data_replaced = true;
                self.profiles_save = None;
                Some((connections, swapped))
            }
            Err(message) => {
                self.stable_import_failed(message, cx);
                None
            }
        }
    }

    fn stable_import_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.stable_import = None;
        self.note(
            format!("Could not import from DBDelve, and nothing was changed. {message}"),
            cx,
        );
    }

    /// Into the same install, which reads the imported directory as it starts.
    pub(crate) fn restart_after_import(&mut self, cx: &mut Context<Self>) {
        update::before_restart();
        if let Some(install) = update::this_install() {
            cx.set_restart_path(install);
        }
        cx.restart();
    }

    /// The dialog's button the ring is on, pressed.
    pub(crate) fn answer_stable_import(&mut self, choice: usize, cx: &mut Context<Self>) {
        match (&self.stable_import, choice) {
            (Some(StableImport::Confirm { .. }), 0) => self.run_stable_import(cx),
            (Some(StableImport::Confirm { .. }), _) => self.cancel_stable_import(cx),
            (Some(StableImport::Done(_)), _) => self.restart_after_import(cx),
            _ => {}
        }
    }

    /// The buttons the dialog in front has for the ring to move across.
    pub(crate) fn stable_import_buttons(&self) -> usize {
        match self.stable_import {
            Some(StableImport::Confirm { .. }) => 2,
            Some(StableImport::Running) | None => 0,
            Some(StableImport::Done(_)) => 1,
        }
    }

    pub(crate) fn render_stable_import(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let name = Channel::current().display_name();
        let text = |line: String| {
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child(line)
        };
        let buttons = || div().flex().justify_end().gap(px(layout::SPACE_SM));
        let card = match self.stable_import.as_ref()? {
            StableImport::Confirm { backup } => dialog(t)
                .child(section_label(t, "Import from DBDelve"))
                .child(text(format!(
                    "{name}'s connections, queries and settings are replaced with DBDelve's. \
                     The ones here now are moved to {}.",
                    backup.display()
                )))
                .child(
                    buttons()
                        .child(
                            self.choice(
                                "stable-import",
                                1,
                                button(
                                    "cancel-stable-import",
                                    "Cancel",
                                    Tone::Quiet,
                                    Control::Standard,
                                    t,
                                )
                                .on_click(cx.listener(
                                    |workspace, _, _, cx| {
                                        workspace.cancel_stable_import(cx);
                                    },
                                )),
                                t,
                            ),
                        )
                        .child(
                            self.choice(
                                "stable-import",
                                0,
                                button(
                                    "confirm-stable-import",
                                    "Import",
                                    Tone::Primary,
                                    Control::Standard,
                                    t,
                                )
                                .on_click(cx.listener(
                                    |workspace, _, _, cx| {
                                        workspace.run_stable_import(cx);
                                    },
                                )),
                                t,
                            ),
                        ),
                ),
            StableImport::Running => dialog(t)
                .child(section_label(t, "Import from DBDelve"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .child(
                            Spinner::new()
                                .icon(IconName::LoaderCircle)
                                .color(t.text_muted.into())
                                .small(),
                        )
                        .child(text(
                            "Copying connections, queries and saved passwords…".into(),
                        )),
                )
                .when(cfg!(target_os = "macos"), |card| {
                    card.child(text(
                        "macOS asks before each saved password is read. Choose Allow to \
                         copy it."
                            .into(),
                    ))
                }),
            StableImport::Done(imported) => dialog(t)
                .child(section_label(t, "Imported from DBDelve"))
                .child(text(format!(
                    "{} imported. {name} restarts to open {}.",
                    match imported.connections {
                        1 => "1 connection".to_string(),
                        count => format!("{count} connections"),
                    },
                    if imported.connections == 1 {
                        "it"
                    } else {
                        "them"
                    }
                )))
                .when(!imported.without_secrets.is_empty(), |card| {
                    card.child(text(format!(
                        "Saved passwords couldn't be copied for {}. Edit {} to enter them \
                         again.",
                        imported.without_secrets.join(", "),
                        if imported.without_secrets.len() == 1 {
                            "that connection"
                        } else {
                            "those connections"
                        }
                    )))
                })
                .children(imported.backup.as_ref().map(|backup| {
                    text(format!("What was here before is in {}.", backup.display()))
                }))
                .child(
                    buttons().child(
                        self.choice(
                            "stable-import",
                            0,
                            button(
                                "restart-after-import",
                                "Restart",
                                Tone::Primary,
                                Control::Standard,
                                t,
                            )
                            .on_click(cx.listener(
                                |workspace, _, _, cx| {
                                    workspace.restart_after_import(cx);
                                },
                            )),
                            t,
                        ),
                    ),
                ),
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }
}

/// Every connection's password and key passphrase, from stable's services
/// into this build's. Returns the names of the connections one didn't come
/// across for: every one, when the services can't even be named.
fn copy_saved_passwords(connections: &[Listed]) -> Vec<String> {
    match Services::stable().and_then(|stable| Ok((stable, Services::this_build()?))) {
        Ok((stable, here)) => {
            stable_import::copy_secrets(connections, &stable, &here, &SystemKeychain)
        }
        Err(error) => {
            tracing::warn!("saved passwords could not be imported: {error}");
            connections
                .iter()
                .map(|connection| connection.name().to_string())
                .collect()
        }
    }
}
