//! Projects: named collections of connections. A connection is in at most one
//! project, and the ones in none form the No project group. The group of the
//! connection in front is the one the titlebar names and the palette and
//! next/previous connection keep to; nothing about a connection itself
//! changes, so its tabs, saved queries and history come back as they were.

use super::*;

impl Workspace {
    /// The project a connection is in, by name, or `None` for No project.
    pub(crate) fn group_of(&self, id: &str) -> Option<&str> {
        self.projects
            .iter()
            .find(|project| project.connections.iter().any(|member| member == id))
            .map(|project| project.name.as_str())
    }

    /// The group of the connection in front.
    pub(crate) fn current_group(&self) -> Option<&str> {
        self.profile()
            .and_then(|profile| self.group_of(&profile.id))
    }

    pub(crate) fn in_current_group(&self, index: usize) -> bool {
        self.profiles
            .get(index)
            .is_some_and(|profile| self.group_of(&profile.id) == self.current_group())
    }

    pub(crate) fn current_group_members(&self) -> Vec<usize> {
        (0..self.profiles.len())
            .filter(|index| self.in_current_group(*index))
            .collect()
    }

    pub(crate) fn is_expanded(&self, group: Option<&str>) -> bool {
        self.expanded_groups
            .iter()
            .any(|expanded| expanded.as_deref() == group)
    }

    /// A click on a group's header in the switcher: expands it to look
    /// inside, or folds it. Nothing is switched to; that takes a click on a
    /// connection.
    pub(crate) fn toggle_group(&mut self, group: Option<String>, cx: &mut Context<Self>) {
        if self.is_expanded(group.as_deref()) {
            self.expanded_groups.retain(|expanded| expanded != &group);
        } else {
            self.expanded_groups.push(group);
        }
        cx.notify();
    }

    /// Opens the switcher with only the group in front expanded, and its
    /// search field focused so typing filters straight away.
    pub(crate) fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_open = true;
        self.pending_removal = None;
        self.pending_project_deletion = None;
        self.assigning_project = None;
        self.expanded_groups = vec![self.current_group().map(str::to_string)];
        self.search_selection = 0;
        let input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(
            &input,
            window,
            |workspace, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    workspace.search_selection = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    if let Some(index) = workspace.selected_connection(cx) {
                        workspace.activate(index, cx);
                    }
                }
                _ => {}
            },
        )
        .detach();
        self.connection_search = Some(input);
        self.connection_search_needs_focus = true;
        cx.notify();
    }

    /// Puts the search field away, handing focus back for the reason
    /// `drop_project_name` gives. Not when the switcher closed for a form: the
    /// tab's focus is applied after the form's, and would take it.
    pub(crate) fn drop_connection_search(&mut self) {
        if self.connection_search.take().is_some()
            && self.form.is_none()
            && let Some(profile) = self.profile_mut()
        {
            profile.session.editor_needs_focus = true;
        }
    }

    /// The match up and down have moved to, held inside the matches as they
    /// narrow.
    pub(crate) fn selected_connection(&self, cx: &App) -> Option<usize> {
        let matches = self.searched_connections(cx)?;
        matches
            .get(self.search_selection.min(matches.len().saturating_sub(1)))
            .copied()
    }

    pub(crate) fn step_search_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(matches) = self.searched_connections(cx) else {
            return;
        };
        let last = matches.len().saturating_sub(1);
        self.search_selection = self
            .search_selection
            .min(last)
            .saturating_add_signed(step)
            .min(last);
        cx.notify();
    }

    /// The connections the switcher's search matches, in the order it lists
    /// them, or `None` while there is no search.
    pub(crate) fn searched_connections(&self, cx: &App) -> Option<Vec<usize>> {
        let query = self
            .connection_search
            .as_ref()?
            .read(cx)
            .value()
            .trim()
            .to_string();
        if query.is_empty() {
            return None;
        }
        let connections = self
            .profiles
            .iter()
            .map(|profile| {
                let host = match &profile.config {
                    ConnectionConfig::Snowflake(snowflake) => snowflake.host(),
                    // A SQLite path has no host, and every one under a home
                    // directory would match its folder names.
                    config => config
                        .server()
                        .map(|server| server.host.clone())
                        .unwrap_or_default(),
                };
                (profile.id.as_str(), profile.name.as_str(), host)
            })
            .collect::<Vec<_>>();
        Some(matching_connections(&connections, &self.projects, &query))
    }

    /// Opens the name field: for a new project with `renaming` empty, or in
    /// place of an existing project's name, prefilled with it.
    pub(crate) fn start_naming_project(
        &mut self,
        renaming: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = renaming.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Project name");
            input.set_value(prefill, window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            |workspace, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let name = input.read(cx).value().trim().to_string();
                    match workspace.renaming_project.clone() {
                        Some(old) => workspace.rename_project(&old, name, cx),
                        None => workspace.create_project(name, cx),
                    }
                }
            },
        )
        .detach();
        self.project_name = Some(input);
        self.renaming_project = renaming;
        self.project_name_needs_focus = true;
        cx.notify();
    }

    /// Names a new project where projects are listed: the switcher, or the
    /// welcome surface while there is no connection to have one.
    pub(crate) fn new_project(
        &mut self,
        _: &NewProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.form = None;
        self.close_settings(window, cx);
        // Opened afresh even when open, since a search hides the field.
        if !self.profiles.is_empty() {
            self.open_switcher(window, cx);
        }
        // The name field is what was asked for, not the search, nor the
        // editor a palette that ran this hands focus back to.
        self.connection_search_needs_focus = false;
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = false;
        }
        self.start_naming_project(None, window, cx);
    }

    /// The project called `name`, made if there is none. Where a name comes
    /// from a form or an import rather than the name field, a taken one is
    /// joined rather than refused.
    pub(crate) fn project_named(&mut self, name: &str) -> &mut store::StoredProject {
        let at = match self
            .projects
            .iter()
            .position(|project| project.name == name)
        {
            Some(at) => at,
            None => {
                self.projects.push(store::StoredProject {
                    name: name.to_string(),
                    ..Default::default()
                });
                self.projects.len() - 1
            }
        };
        &mut self.projects[at]
    }

    /// Whether `name` cannot be given to a project, saying why when it
    /// cannot. `keeping` is the project's current name on a rename, which it
    /// may keep.
    fn name_refused(&mut self, name: &str, keeping: Option<&str>, cx: &mut Context<Self>) -> bool {
        let message = if name.is_empty() {
            "A project needs a name.".to_string()
        } else if Some(name) != keeping && self.projects.iter().any(|project| project.name == name)
        {
            format!("A project named {name} already exists.")
        } else {
            return false;
        };
        self.note(message, cx);
        self.welcome_notice_refuses_name = self.profiles.is_empty() && self.form.is_none();
        true
    }

    fn rename_project(&mut self, old: &str, name: String, cx: &mut Context<Self>) {
        if self.name_refused(&name, Some(old), cx) {
            return;
        }
        for expanded in self.expanded_groups.iter_mut().flatten() {
            if expanded == old {
                *expanded = name.clone();
            }
        }
        if let Some(project) = self.projects.iter_mut().find(|project| project.name == old) {
            project.name = name;
        }
        self.drop_project_name();
        self.remember_profiles(cx);
        cx.notify();
    }

    fn create_project(&mut self, name: String, cx: &mut Context<Self>) {
        if self.name_refused(&name, None, cx) {
            return;
        }
        self.drop_project_name();
        self.expanded_groups.push(Some(name.clone()));
        self.projects.push(store::StoredProject {
            name,
            ..Default::default()
        });
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Puts the name field away. It held focus while open, and a field
    /// unmounted with focus in it takes every keybinding with it, so focus
    /// goes back to the search field if the switcher stays open, else to
    /// whatever is in front -- unless that is a form, which hands focus to a
    /// field of its own that the tab's focus, applied after it, would take.
    pub(crate) fn drop_project_name(&mut self) {
        self.renaming_project = None;
        if self.project_name.take().is_none() {
            return;
        }
        if std::mem::take(&mut self.welcome_notice_refuses_name) {
            self.welcome_notice = None;
        }
        if self.switcher_open && self.connection_search.is_some() {
            self.connection_search_needs_focus = true;
        } else if self.form.is_none()
            && let Some(profile) = self.profile_mut()
        {
            profile.session.editor_needs_focus = true;
        }
    }

    /// The project goes; its connections stay, under No project. The first
    /// click only arms it, as removing a connection does: a project's
    /// membership is not something to lose to a click aimed at its header.
    pub(crate) fn delete_project(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.pending_project_deletion.as_deref() != Some(name) {
            self.pending_project_deletion = Some(name.to_string());
            cx.notify();
            return;
        }
        self.pending_project_deletion = None;
        self.projects.retain(|project| project.name != name);
        self.expanded_groups
            .retain(|expanded| expanded.as_deref() != Some(name));
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Moves a connection into `project`, or with `None` out of every project
    /// and into No project.
    pub(crate) fn move_to_project(
        &mut self,
        index: usize,
        project: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.profiles.get(index).map(|profile| profile.id.clone()) else {
            return;
        };
        for each in &mut self.projects {
            each.connections.retain(|member| member != &id);
            if Some(each.name.as_str()) == project {
                each.connections.push(id.clone());
            }
        }
        self.assigning_project = None;
        self.remember_profiles(cx);
        cx.notify();
    }
}

/// Projects as read from disk, held to what the switcher assumes: names are
/// unique, every id names a live connection, and each connection is in one
/// project at most, the first that lists it. A hand-edited file, or one an
/// earlier build wrote, can break any of them, and a stale id would quietly
/// claim the next connection given the same name.
pub(crate) fn normalized_projects(
    mut projects: Vec<store::StoredProject>,
    live: &[&str],
) -> Vec<store::StoredProject> {
    let mut seen = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for project in &mut projects {
        project
            .connections
            .retain(|id| live.contains(&id.as_str()) && seen.insert(id.clone()));
        let base = project.name.clone();
        let mut suffix = 2;
        while !names.insert(project.name.clone()) {
            project.name = format!("{base} {suffix}");
            suffix += 1;
        }
    }
    projects
}

/// The project a saved connection form puts its connection in: the one named
/// in its new-project field when that is open, which joins a project already
/// called that rather than refusing the name, or else the one picked, unless
/// it was deleted while the form sat open.
pub(crate) fn chosen_project(
    typed: Option<&str>,
    picked: Option<&str>,
    projects: &[store::StoredProject],
) -> Result<Option<String>, String> {
    match typed.map(str::trim) {
        Some("") => Err("A project needs a name.".to_string()),
        Some(name) => Ok(Some(name.to_string())),
        None => Ok(picked
            .filter(|picked| projects.iter().any(|project| project.name == *picked))
            .map(str::to_string)),
    }
}

/// The `(id, name, host)` connections whose name, host or project holds
/// `query`, ignoring case, in the order the switcher lists them: No project
/// first, then each project's.
fn matching_connections(
    connections: &[(&str, &str, String)],
    projects: &[store::StoredProject],
    query: &str,
) -> Vec<usize> {
    let query = query.to_lowercase();
    let holds = |text: &str| text.to_lowercase().contains(&query);
    let mut matches = connections
        .iter()
        .enumerate()
        .filter_map(|(index, (id, name, host))| {
            let project = projects
                .iter()
                .position(|project| project.connections.iter().any(|member| member == id));
            (holds(name) || holds(host) || project.is_some_and(|at| holds(&projects[at].name)))
                .then_some((project.map_or(0, |at| at + 1), index))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|&(group, _)| group);
    matches.into_iter().map(|(_, index)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, connections: &[&str]) -> store::StoredProject {
        store::StoredProject {
            name: name.into(),
            connections: connections.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn a_loaded_project_with_a_taken_name_is_renamed_rather_than_merged() {
        let projects = normalized_projects(
            vec![
                project("Billing", &[]),
                project("Billing", &[]),
                project("Billing 2", &[]),
            ],
            &[],
        );
        let names = projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Billing", "Billing 2", "Billing 2 2"]);
    }

    #[test]
    fn a_loaded_project_keeps_only_live_connections_each_in_its_first_project() {
        let projects = normalized_projects(
            vec![
                project("Billing", &["dev", "gone", "prod"]),
                project("Analytics", &["prod", "warehouse"]),
            ],
            &["dev", "prod", "warehouse"],
        );
        assert_eq!(
            projects,
            vec![
                project("Billing", &["dev", "prod"]),
                project("Analytics", &["warehouse"]),
            ]
        );
    }

    #[test]
    fn a_form_puts_its_connection_in_the_project_typed_or_picked() {
        let projects = [project("Billing", &[])];
        assert_eq!(
            chosen_project(Some("  Analytics "), Some("Billing"), &projects),
            Ok(Some("Analytics".to_string()))
        );
        assert_eq!(
            chosen_project(Some("Billing"), None, &projects),
            Ok(Some("Billing".to_string()))
        );
        assert!(chosen_project(Some("  "), Some("Billing"), &projects).is_err());
        assert_eq!(
            chosen_project(None, Some("Billing"), &projects),
            Ok(Some("Billing".to_string()))
        );
        assert_eq!(chosen_project(None, Some("Deleted"), &projects), Ok(None));
    }

    #[test]
    fn a_search_matches_names_in_any_case_and_lists_them_as_the_switcher_does() {
        let connections = [
            ("billing-prod", "Billing prod", String::new()),
            ("scratch", "Scratch", String::new()),
            ("analytics-prod", "Analytics PROD", String::new()),
            ("local-prod", "local prod", String::new()),
        ];
        let projects = [
            project("Analytics", &["analytics-prod"]),
            project("Billing", &["billing-prod"]),
        ];
        assert_eq!(
            matching_connections(&connections, &projects, "Prod"),
            [3, 2, 0]
        );
    }

    #[test]
    fn a_search_matches_a_connection_by_its_host_or_its_project() {
        let connections = [
            ("orders", "Orders", "db.internal".to_string()),
            ("ledger", "Ledger", "localhost".to_string()),
            ("scratch", "Scratch", "localhost".to_string()),
        ];
        let projects = [project("Billing", &["ledger"])];
        assert_eq!(
            matching_connections(&connections, &projects, "INTERNAL"),
            [0]
        );
        assert_eq!(matching_connections(&connections, &projects, "bill"), [1]);
    }
}
