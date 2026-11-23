//! Moving the saved queries out of the support directory into a folder the
//! user picks, and the notice that offers it.
//!
//! The picker is the only way in, so macOS asks for access to the one folder
//! picked and never to Documents at launch.

use std::path::{Path, PathBuf};

use gpui::PathPromptOptions;
use gpui_component::button::Button;

use super::*;

use crate::sql_file::{self, Move};

/// The line under the titlebar about the saved queries' move.
pub(crate) enum QueriesNotice {
    /// The offer, drawn while any connection has a saved query left.
    Offer,
    Report {
        moved: usize,
        skipped: usize,
        failed: usize,
        folder: PathBuf,
    },
}

impl Workspace {
    fn saved_query_count(&self) -> usize {
        self.profiles
            .iter()
            .map(|profile| profile.session.saved_queries.len())
            .sum()
    }

    /// Whether any connection still has saved queries in the support
    /// directory, which is all the palette's command waits on.
    pub(crate) fn has_saved_queries(&self) -> bool {
        self.saved_query_count() > 0
    }

    /// The offer, on a launch with saved queries left, until it is turned
    /// down or taken.
    pub(crate) fn offer_to_move_queries(&mut self) {
        if !self.settings.queries_move_dismissed && self.has_saved_queries() {
            self.queries_notice = Some(QueriesNotice::Offer);
        }
    }

    /// Move to Folder…: the folder picker, then the move.
    pub(crate) fn pick_queries_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Move Here".into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let picked = picked.await;
            _ = workspace.update_in(cx, |workspace, window, cx| match picked {
                Ok(Ok(Some(paths))) => {
                    if let Some(folder) = paths.first() {
                        workspace.move_saved_queries(folder, cx);
                        // The report has no Move button, which may hold focus.
                        workspace.focus.focus(window, cx);
                        workspace.refocus_front();
                    }
                }
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    /// Every connection's saved queries to `<folder>/<connection>/<name>.sql`,
    /// each connection in a folder of its own even where two names would
    /// meet. A name taken there is skipped and its query left where it was.
    /// A tab on a moved file follows it, on whichever connection holds it,
    /// with any unsaved text still unsaved and its scratch file where it was.
    /// A profile this build cannot read is not a connection here, so its
    /// queries stay.
    ///
    /// ponytail: copied on the window's thread, as Save writes, so a slow
    /// folder holds the window until the copies land; that is also what keeps
    /// a Save to a query's old file from landing between its copy and its
    /// removal. The blocking pool is the upgrade, with Save held off the
    /// files in flight.
    pub(crate) fn move_saved_queries(&mut self, folder: &Path, cx: &mut Context<Self>) {
        let folder = sql_file::canonical(folder);
        let names: Vec<&str> = self
            .profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect();
        let subfolders = store::connection_folders(&names);
        let (mut moved, mut skipped, mut failed) = (0, 0, 0);
        let mut copied = Vec::new();
        let mut owners = Vec::new();
        for (index, subfolder) in subfolders.into_iter().enumerate() {
            let queries = self.profiles[index].session.saved_queries.clone();
            if queries.is_empty() {
                continue;
            }
            owners.push(self.profiles[index].id.clone());
            let directory = folder.join(subfolder);
            if let Err(error) = sql_file::create_dirs(&directory) {
                tracing::debug!(%error, "a connection's folder for its queries was not made");
                failed += queries.len();
                continue;
            }
            for name in queries {
                // The entry itself, not where it leads: a query that is a link
                // moves as the text it reads, and the file it pointed at stays.
                let from = self.profiles[index]
                    .session
                    .query_directory
                    .join(format!("{name}.sql"));
                let to = directory.join(format!("{name}.sql"));
                // A tab stays bound to a file deleted on disk, and the copy
                // would give the query's own tab that path too.
                if self.tab_on_file(&sql_file::canonical(&to)).is_some() {
                    skipped += 1;
                    continue;
                }
                let fresh = self.seen_as_it_is(&from);
                match sql_file::copy_new(&from, &to) {
                    Move::Done => copied.push((index, name, from, to, fresh)),
                    Move::Taken => skipped += 1,
                    Move::Failed(error) => {
                        tracing::debug!(%error, "a saved query was not copied");
                        failed += 1;
                    }
                }
            }
        }
        // Only once every copy is on disk does any original go: a query that
        // is a link to another reads nothing once that one has gone.
        let mut holders = Vec::new();
        for (index, name, from, to, fresh) in copied {
            if let Err(error) = sql_file::remove_moved(&from, &to) {
                tracing::debug!(%error, "a saved query was not moved");
                failed += 1;
                continue;
            }
            moved += 1;
            let to = sql_file::canonical(&to);
            holders.extend(self.repoint_file_tab(&from, &to, fresh));
            for profile in &mut self.profiles {
                if let Some(held) = profile.recent_files.iter_mut().find(|held| **held == from) {
                    *held = to.clone();
                    profile.recent_files_changed();
                    holders.push(profile.id.clone());
                }
            }
            let profile = &mut self.profiles[index];
            profile.shortcuts.forget_query(&name);
            crate::session::remember_recent(&mut profile.recent_files, to);
            profile.recent_files_changed();
        }
        for id in &owners {
            self.reload_queries_of(id, cx);
        }
        // A connection none of whose queries moved lists a moved file where a
        // tab or its recent files followed one there.
        holders.retain(|holder| !owners.contains(holder));
        holders.sort();
        holders.dedup();
        for id in holders {
            self.refresh_query_library(&id, cx);
        }
        self.settings.queries_move_dismissed = true;
        self.queries_notice = Some(QueriesNotice::Report {
            moved,
            skipped,
            failed,
            folder,
        });
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Whether the tab on `path`, if any, last read or wrote the file as it
    /// is now. Only then does it take the copy's stamp: one that had missed
    /// a change keeps missing it, so the next look still settles it.
    fn seen_as_it_is(&self, path: &Path) -> bool {
        let Some((index, id)) = self.tab_on_file(path) else {
            return false;
        };
        let stamp = self.profiles[index]
            .session
            .query_tab(id)
            .and_then(|tab| tab.file.as_ref()?.stamp);
        stamp.is_some() && stamp == sql_file::stamp(path).ok().flatten()
    }

    /// The tab bound to `from`, on whichever connection, bound to `to`
    /// instead where its chip stands. The connection holding it, if any.
    fn repoint_file_tab(&mut self, from: &Path, to: &Path, fresh: bool) -> Option<String> {
        let (index, id) = self.tab_on_file(from)?;
        let session = &mut self.profiles[index].session;
        session.rekey(
            &TabKey::File(from.to_path_buf()),
            TabKey::File(to.to_path_buf()),
        );
        if let Some(file) = session.query_tab_mut(id).and_then(|tab| tab.file.as_mut()) {
            file.path = to.to_path_buf();
            if fresh {
                file.stamp = sql_file::stamp(to).ok().flatten();
            }
        }
        Some(self.profiles[index].id.clone())
    }

    /// Not Now, or Dismiss on the report. The offer is not made again;
    /// the palette's command stays while there are queries to move.
    fn dismiss_queries_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.queries_notice = None;
        if !self.settings.queries_move_dismissed {
            self.settings.queries_move_dismissed = true;
            self.remember_profiles(cx);
        }
        // A button tabbed to holds focus as the notice unmounts.
        self.focus.focus(window, cx);
        self.refocus_front();
        cx.notify();
    }

    pub(crate) fn render_queries_notice(
        &self,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let (line, reveal) = match self.queries_notice.as_ref()? {
            QueriesNotice::Offer => {
                let line = match self.saved_query_count() {
                    0 => return None,
                    1 => "Your saved query is now a file.".to_string(),
                    count => format!("Your {count} saved queries are now files."),
                };
                let queries = store::dbdelve_directory().ok()?.join("queries");
                (line, queries)
            }
            QueriesNotice::Report {
                moved,
                skipped,
                failed,
                folder,
            } => (
                format!("Moved {moved}, skipped {skipped}, failed {failed}."),
                folder.clone(),
            ),
        };
        let offer = matches!(self.queries_notice, Some(QueriesNotice::Offer));
        let framed = |button: Button| {
            div()
                .rounded(px(layout::RADIUS_CONTROL))
                .border_1()
                .border_color(t.border)
                .bg(t.element_active)
                .child(button)
        };
        let button = |id: &'static str, label: &'static str| {
            ui::button(id, label, Tone::Primary, Control::Compact, t)
                .debug_selector(move || id.to_string())
        };
        let action =
            |id,
             label,
             act: fn(&mut Self, &mut Window, &mut Context<Self>),
             cx: &mut Context<Self>| {
                framed(button(id, label).on_click(
                    cx.listener(move |workspace, _, window, cx| act(workspace, window, cx)),
                ))
            };
        let dismiss = if offer { "Not Now" } else { "Dismiss" };
        Some(
            div()
                .flex_shrink_0()
                .w_full()
                .px(px(layout::SPACE_MD))
                .py(px(layout::SPACE_SM))
                .border_b_1()
                .border_color(t.border)
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child(div().debug_selector(|| "queries-notice".into()).child(line))
                .when(offer, |row| {
                    row.child(action(
                        "queries-notice-move",
                        "Move to Folder…",
                        |workspace, window, cx| workspace.pick_queries_folder(window, cx),
                        cx,
                    ))
                })
                .child(framed(
                    button("queries-notice-reveal", "Reveal")
                        .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                ))
                .child(action(
                    "queries-notice-dismiss",
                    dismiss,
                    Self::dismiss_queries_notice,
                    cx,
                )),
        )
    }
}
