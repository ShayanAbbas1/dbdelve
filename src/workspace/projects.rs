//! Projects: named collections of connections. A connection is in at most one
//! project, and the ones in none form the No project group. Opening a group
//! narrows the switcher, the palette and next/previous connection to its
//! members; nothing about a connection itself changes, so its tabs, saved
//! queries and history come back exactly as they were.

use super::*;

impl Workspace {
    pub(crate) fn open_project(&self) -> Option<&store::StoredProject> {
        self.projects.iter().find(|project| project.open)
    }

    /// Whether the profile at `index` is in the open group: the open project,
    /// or with none open, no project at all.
    pub(crate) fn in_project(&self, index: usize) -> bool {
        let Some(profile) = self.profiles.get(index) else {
            return false;
        };
        match self.open_project() {
            Some(project) => project.connections.contains(&profile.id),
            None => self.project_of(&profile.id).is_none(),
        }
    }

    pub(crate) fn project_of(&self, id: &str) -> Option<&store::StoredProject> {
        self.projects
            .iter()
            .find(|project| project.connections.iter().any(|member| member == id))
    }

    pub(crate) fn project_members(&self) -> Vec<usize> {
        (0..self.profiles.len())
            .filter(|index| self.in_project(*index))
            .collect()
    }

    /// `None` closes whatever project is open, opening the No project group.
    pub(crate) fn switch_project(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for project in &mut self.projects {
            project.open = Some(project.name.as_str()) == name;
        }
        self.project_collapsed = false;
        self.pending_removal = None;
        self.remember_profiles(cx);
        self.settle_project(window, cx);
        cx.notify();
    }

    /// A click on a group's header: the selected group folds or unfolds,
    /// any other is selected, which expands it and folds the rest.
    pub(crate) fn select_group(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_project().map(|project| project.name.as_str()) == name {
            self.project_collapsed = !self.project_collapsed;
            cx.notify();
        } else {
            self.switch_project(name, window, cx);
        }
    }

    /// Puts a member of the open project in front if the one there is not.
    /// An empty project has nothing to put there, so it asks for its first
    /// connection, which `create_profile` adds to it.
    // ponytail: cancelling that form shows the previous connection again,
    // outside the project; an empty pane would need `active` to become optional.
    fn settle_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_project(self.active) {
            return;
        }
        match self.project_members().first() {
            Some(&index) => self.activate(index, cx),
            None => {
                self.switcher_open = false;
                self.form = Some(ConnectionForm::new(None, window, cx));
            }
        }
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
            |workspace, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let name = input.read(cx).value().trim().to_string();
                    match workspace.renaming_project.clone() {
                        Some(old) => workspace.rename_project(&old, name, cx),
                        None => workspace.create_project(name, window, cx),
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

    fn name_taken(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let taken = self.projects.iter().any(|project| project.name == name);
        if taken {
            self.note(format!("A project named {name} already exists."), cx);
        }
        taken
    }

    fn rename_project(&mut self, old: &str, name: String, cx: &mut Context<Self>) {
        if name.is_empty() || (name != old && self.name_taken(&name, cx)) {
            return;
        }
        if let Some(project) = self.projects.iter_mut().find(|project| project.name == old) {
            project.name = name;
        }
        self.project_name = None;
        self.renaming_project = None;
        self.remember_profiles(cx);
        cx.notify();
    }

    fn create_project(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if name.is_empty() || self.name_taken(&name, cx) {
            return;
        }
        self.project_name = None;
        self.projects.push(store::StoredProject {
            name: name.clone(),
            ..Default::default()
        });
        self.switch_project(Some(&name), window, cx);
    }

    /// The project goes; its connections stay, under No project.
    pub(crate) fn delete_project(&mut self, name: &str, cx: &mut Context<Self>) {
        self.projects.retain(|project| project.name != name);
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Moves a connection into `project`, or with `None` out of every project
    /// and into No project. The connection in front is followed into its new
    /// group rather than left in front of a group that no longer lists it.
    pub(crate) fn move_to_project(
        &mut self,
        index: usize,
        project: Option<&str>,
        window: &mut Window,
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
        if index == self.active {
            self.switch_project(project, window, cx);
        } else {
            self.remember_profiles(cx);
            cx.notify();
        }
    }
}
