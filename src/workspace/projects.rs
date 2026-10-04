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
    /// goes back to whatever is in front.
    pub(crate) fn drop_project_name(&mut self) {
        self.renaming_project = None;
        if self.project_name.take().is_some()
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
}
