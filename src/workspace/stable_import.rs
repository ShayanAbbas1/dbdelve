//! Import from DBDelve, offered by a nightly only: a stable install's
//! connections, saved passwords, queries and settings copied beside this
//! one's, and put in place by the next launch (`settle_at_launch`), which
//! then copies the saved passwords.
//!
//! A restart rather than a reload in place: the plugins, the user themes and
//! the keymap are all read once, at launch, before the workspace exists, and
//! an import can bring each of them. Nothing is moved while this session
//! runs, so it goes on saving into the directory that becomes the backup.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gpui_component::spinner::Spinner;
use gpui_component::{IconName, Sizable};

use super::*;
use crate::store::stable_import::{self, Services, Staged, SystemKeychain};

/// Where an import is, as its dialog shows it.
pub(crate) enum StableImport {
    /// Asked first, since the data here is moved aside to `backup`, the
    /// name free `at` the time asked, which the copy then names again.
    Confirm {
        backup: PathBuf,
        at: SystemTime,
    },
    Running,
    Done(Imported),
}

pub(crate) struct Imported {
    /// Holds the import's lock until this process exits.
    staged: Staged,
    /// What to restart into, or why this install can't start itself again.
    relaunch: Result<Option<PathBuf>, &'static str>,
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
        let at = SystemTime::now();
        if target.exists() {
            self.stable_import = Some(StableImport::Confirm {
                backup: stable_import::free_backup(&target, at),
                at,
            });
            cx.notify();
        } else {
            self.run_stable_import(at, cx);
        }
    }

    pub(crate) fn cancel_stable_import(&mut self, cx: &mut Context<Self>) {
        if matches!(self.stable_import, Some(StableImport::Confirm { .. })) {
            self.stable_import = None;
            cx.notify();
        }
    }

    /// Off this thread, as the copy can be large. The saved passwords wait
    /// for the launch that puts the import in place
    /// ([`Workspace::copy_imported_passwords`]), so none here is replaced for
    /// an import that never goes in.
    pub(crate) fn run_stable_import(&mut self, at: SystemTime, cx: &mut Context<Self>) {
        let directories =
            stable_import::source().and_then(|source| Ok((source, store::dbdelve_directory()?)));
        let (source, target) = match directories {
            Ok(directories) => directories,
            Err(message) => return self.stable_import_failed(message, cx),
        };
        self.stable_import = Some(StableImport::Running);
        cx.notify();
        let imported = spawn_blocking(cx.background_executor(), move || {
            let staged = stable_import::stage(&source, &target, at)?;
            Ok::<_, String>(Imported {
                staged,
                relaunch: update::this_install(),
            })
        });
        cx.spawn(async move |workspace, cx| {
            let imported = imported.await;
            _ = workspace.update(cx, |workspace, cx| match imported {
                Ok(imported) => {
                    workspace.stable_import = Some(StableImport::Done(imported));
                    cx.notify();
                }
                Err(message) => workspace.stable_import_failed(message, cx),
            });
        })
        .detach();
    }

    /// For the import this launch put in place, once its window is up: on
    /// macOS each password can raise a Keychain prompt, which then has the
    /// app behind it. Off this thread, as each read can wait on one.
    pub(crate) fn copy_imported_passwords(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        let failed = spawn_blocking(cx.background_executor(), move || {
            copy_saved_passwords(&target)
        });
        cx.spawn(async move |workspace, cx| {
            let failed = failed.await;
            if failed.is_empty() {
                return;
            }
            _ = workspace.update(cx, |workspace, cx| {
                workspace.note(
                    format!(
                        "Saved passwords couldn't be copied for {}. Edit {} to enter them \
                         again.",
                        failed.join(", "),
                        if failed.len() == 1 {
                            "that connection"
                        } else {
                            "those connections"
                        }
                    ),
                    cx,
                );
            });
        })
        .detach();
    }

    fn stable_import_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.stable_import = None;
        self.note(
            format!("Could not import from DBDelve, and nothing was changed. {message}"),
            cx,
        );
    }

    /// Into the same install, the way an update restarts, which puts the
    /// import in place as it starts. One that can't start itself again
    /// quits, and the import waits for whenever it is opened next.
    pub(crate) fn restart_after_import(&mut self, cx: &mut Context<Self>) {
        let Some(StableImport::Done(imported)) = &self.stable_import else {
            return;
        };
        match imported.relaunch {
            Ok(Some(ref install)) => restart_into(install.clone(), cx),
            Ok(None) => {
                update::before_restart();
                cx.restart();
            }
            Err(_) => cx.quit(),
        }
    }

    /// The dialog's button the ring is on, pressed.
    pub(crate) fn answer_stable_import(&mut self, choice: usize, cx: &mut Context<Self>) {
        match (&self.stable_import, choice) {
            (Some(StableImport::Confirm { at, .. }), 0) => self.run_stable_import(*at, cx),
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
            StableImport::Confirm { backup, .. } => dialog(t)
                .child(section_label(t, "Import from DBDelve"))
                .child(text(format!(
                    "{name}'s connections, queries and settings are replaced with DBDelve's \
                     when it restarts. The ones here now are moved to {}.",
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
                                        workspace.answer_stable_import(0, cx);
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
                        .child(text("Copying connections, queries and settings…".into())),
                ),
            StableImport::Done(imported) => dialog(t)
                .child(section_label(t, "Imported from DBDelve"))
                .child(text(format!(
                    "{} copied. {}",
                    match imported.staged.connections.len() {
                        1 => "1 connection".to_string(),
                        count => format!("{count} connections"),
                    },
                    match imported.relaunch {
                        Ok(_) => format!("{name} restarts to put them in place."),
                        Err(_) => format!(
                            "They are put in place the next time {name} starts. Open it \
                             again once it has quit."
                        ),
                    }
                )))
                .child(text(format!(
                    "Saved passwords are copied after it {}{}",
                    match imported.relaunch {
                        Ok(_) => "restarts",
                        Err(_) => "starts again",
                    },
                    if cfg!(target_os = "macos") {
                        "; macOS asks before each one is read."
                    } else {
                        "."
                    }
                )))
                .children(imported.staged.backup.as_ref().map(|backup| {
                    text(format!(
                        "What is here now is moved to {}.",
                        backup.display()
                    ))
                }))
                .child(
                    buttons().child(
                        self.choice(
                            "stable-import",
                            0,
                            button(
                                "restart-after-import",
                                match imported.relaunch {
                                    Ok(_) => "Restart",
                                    Err(_) => "Quit",
                                },
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

/// The saved passwords of the import now in `target`, from stable's services
/// into this build's. Returns the names of the connections one didn't come
/// across for: every one, when the services can't even be named, which
/// leaves them to try again at the next launch.
fn copy_saved_passwords(target: &Path) -> Vec<String> {
    match Services::stable().and_then(|stable| Ok((stable, Services::this_build()?))) {
        Ok((stable, here)) => {
            stable_import::copy_pending_secrets(target, &stable, &here, &SystemKeychain)
        }
        Err(error) => {
            tracing::warn!("saved passwords could not be imported: {error}");
            stable_import::listed(target)
                .iter()
                .map(|connection| connection.name().to_string())
                .collect()
        }
    }
}
