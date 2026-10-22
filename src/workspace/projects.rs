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

    /// The titlebar's connection name, and `SwitchConnection`: the palette
    /// over every connection, by project.
    pub(crate) fn switch_connection(
        &mut self,
        _: &SwitchConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_connections(window, cx);
    }

    /// Folds or unfolds a project in the palette's Connections scope.
    pub(crate) fn toggle_project(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(project) = self.projects.get(at) else {
            return;
        };
        if !self.collapsed_projects.remove(&project.name) {
            self.collapsed_projects.insert(project.name.clone());
        }
        cx.notify();
    }

    pub(crate) fn open_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(PaletteMode::Search(Scope::Connections), window, cx);
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
        self.project_name_error = None;
        self.pending_project_deletion = None;
        cx.notify();
    }

    /// Names a new project: in the palette, or on the welcome surface while
    /// there is no connection to have one.
    pub(crate) fn new_project(
        &mut self,
        _: &NewProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing is named while the import is asking which project it joins,
        // and the welcome surface's field below is not the palette's to stop.
        if self.pending_import.is_some() {
            return;
        }
        if !self.profiles.is_empty() {
            self.open_palette(PaletteMode::NameProject(None), window, cx);
            return;
        }
        self.close_settings(window, cx);
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
        let Some(message) = project_name_refusal(
            name,
            keeping,
            self.projects.iter().map(|project| project.name.as_str()),
        ) else {
            return false;
        };
        if self.profiles.is_empty() && self.form.is_none() {
            self.project_name_error = Some(message);
            cx.notify();
        } else {
            self.note(message, cx);
        }
        true
    }

    pub(crate) fn rename_project(&mut self, old: &str, name: String, cx: &mut Context<Self>) {
        if self.name_refused(&name, Some(old), cx) {
            return;
        }
        if self.collapsed_projects.remove(old) {
            self.collapsed_projects.insert(name.clone());
        }
        if let Some(project) = self.projects.iter_mut().find(|project| project.name == old) {
            project.name = name;
        }
        self.drop_project_name();
        self.remember_profiles(cx);
        cx.notify();
    }

    pub(crate) fn create_project(&mut self, name: String, cx: &mut Context<Self>) {
        if self.name_refused(&name, None, cx) {
            return;
        }
        self.drop_project_name();
        self.projects.push(store::StoredProject {
            name,
            ..Default::default()
        });
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Puts the welcome surface's name field away. It held focus while open,
    /// and a field unmounted with focus in it takes every keybinding with it,
    /// so focus goes back to whatever is in front -- unless that is a form,
    /// which hands focus to a field of its own that the tab's focus, applied
    /// after it, would take.
    pub(crate) fn drop_project_name(&mut self) {
        self.renaming_project = None;
        if self.project_name.take().is_none() {
            return;
        }
        self.project_name_error = None;
        if self.form.is_none() {
            self.refocus_front();
        }
    }

    /// The welcome surface's delete button. The first click only arms it: a
    /// project is not something to lose to a click aimed at its row.
    pub(crate) fn delete_project(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.pending_project_deletion.as_deref() != Some(name) {
            self.pending_project_deletion = Some(name.to_string());
            cx.notify();
            return;
        }
        self.remove_project(name, cx);
    }

    /// The project goes; its connections stay, under No project.
    pub(crate) fn remove_project(&mut self, name: &str, cx: &mut Context<Self>) {
        self.pending_project_deletion = None;
        self.collapsed_projects.remove(name);
        self.projects.retain(|project| project.name != name);
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
        // Moving it where it is would put it last among its project's members
        // and write the file for nothing.
        if self.group_of(&id) == project {
            return;
        }
        for each in &mut self.projects {
            each.connections.retain(|member| member != &id);
            if Some(each.name.as_str()) == project {
                each.connections.push(id.clone());
            }
        }
        self.remember_profiles(cx);
        cx.notify();
    }
}

/// Why `name` cannot be given to a project, if it cannot: it is empty, or
/// another project in `taken` has it. `keeping` is the project's current name
/// on a rename, which it may keep.
pub(crate) fn project_name_refusal<'a>(
    name: &str,
    keeping: Option<&str>,
    mut taken: impl Iterator<Item = &'a str>,
) -> Option<String> {
    if name.is_empty() {
        Some("A project needs a name.".to_string())
    } else if Some(name) != keeping && taken.any(|other| other == name) {
        Some(format!("A project named {name} already exists."))
    } else {
        None
    }
}

/// Projects as read from disk, held to what the palette assumes: names are
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
    fn a_project_name_is_refused_when_empty_or_taken_but_may_be_kept() {
        let taken = ["Billing", "Analytics"];
        assert!(project_name_refusal("", None, taken.into_iter()).is_some());
        assert!(project_name_refusal("Billing", None, taken.into_iter()).is_some());
        assert_eq!(
            project_name_refusal("Billing", Some("Billing"), taken.into_iter()),
            None
        );
        assert!(project_name_refusal("Analytics", Some("Billing"), taken.into_iter()).is_some());
        assert_eq!(project_name_refusal("Ops", None, taken.into_iter()), None);
    }
}
