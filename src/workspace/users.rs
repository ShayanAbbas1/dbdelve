//! Users and roles: reading a server's logins and roles, the tab that lists
//! them, the role changes it puts up for review, and the Show Grants dialog.
//!
//! Every read runs on a side connection ([`crate::db::Connection::apart`]),
//! so a failed catalog read never lands in a transaction the user has open:
//! the Users and roles tab's own, kept while it is open ([`RolesSide`]).
//! Every change is a statement `db::roles` writes, passed through
//! [`sql::is_generated_role_change`], shown whole in the apply review, and
//! run by `execute_and_then`, so the connection's mode decides whether it
//! runs; a role change needs Full. It runs on a side connection too
//! ([`crate::db::Connection::generated_apart`]): a transaction open on the
//! connection, Manual's included, never holds it.

use std::io::Write as _;
use std::sync::{Mutex, PoisonError};

use super::*;

use crate::db::roles::{self, NewRole, ObjectGrants, Role, RoleEdit, RoleGrants};
use crate::sidebar::RolesListing;
use crate::users_view::{Loaded, UsersEvent, UsersView, reports_csv};

use super::exports::{choose_path, could_not_write, write_replacing};

/// Refuses a second role change while one is running on the tab.
pub(super) const ROLE_CHANGE_RUNNING: &str =
    "A role change is still running. Wait for it, or cancel it, first.";

/// The side session a Users and roles tab reads on: opened by its first read
/// and kept, so a click on a role is a query and not a connect -- TCP, TLS,
/// authentication and any SSH route, and one more of a login's `CONNECTION
/// LIMIT` each time. Dropped with the tab, which closes it. A session of an
/// earlier connect of the profile is not used: the profile may have moved
/// to another database since, and reach is read in the database it is on.
#[derive(Default)]
pub(crate) struct RolesSide(Mutex<Option<(u64, Connection)>>);

impl RolesSide {
    /// `read` on the kept session, opened from `connection` first where
    /// there is none of `generation`. A read on one that was lost lets it go,
    /// so the next opens another. One read at a time, as a session runs them.
    pub(crate) fn read<T>(
        &self,
        connection: &Connection,
        generation: u64,
        read: impl FnOnce(&Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let session = match &*kept {
            Some((opened, session)) if *opened == generation => session.clone(),
            _ => {
                let session = connection.apart()?;
                *kept = Some((generation, session.clone()));
                session
            }
        };
        let result = read(&session);
        if result.is_err() && session.is_lost() {
            *kept = None;
        }
        result
    }

    /// Whether it holds a session, of any connect.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// `read` on `side` where the profile has a Users and roles tab, else on a
/// session of its own.
fn on_side<T>(
    side: Option<Arc<RolesSide>>,
    connection: &Connection,
    generation: u64,
    read: impl FnOnce(&Connection) -> Result<T, DbError>,
) -> Result<T, DbError> {
    match side {
        Some(side) => side.read(connection, generation, read),
        None => read(&connection.apart()?),
    }
}

/// What the role form is for.
#[derive(Clone)]
pub(crate) enum RoleFormKind {
    New { login: bool },
    Edit(Role),
    Password(String),
}

/// The New User, New Role, Edit and Reset Password dialog.
pub(crate) struct RoleForm {
    pub(crate) profile: String,
    pub(crate) kind: RoleFormKind,
    pub(crate) name: Entity<InputState>,
    pub(crate) password: Entity<InputState>,
    pub(crate) confirm: Entity<InputState>,
    pub(crate) limit: Entity<InputState>,
    pub(crate) expires: Entity<InputState>,
    pub(crate) member_of: Entity<InputState>,
    pub(crate) error: Option<String>,
}

impl RoleForm {
    pub(crate) fn title(&self) -> String {
        match &self.kind {
            RoleFormKind::New { login: true } => "New User".into(),
            RoleFormKind::New { login: false } => "New Role".into(),
            RoleFormKind::Edit(role) => format!("Edit {}", role.name),
            RoleFormKind::Password(name) => format!("Reset the password of {name}"),
        }
    }
}

/// Whose grants the Show Grants dialog lists.
pub(crate) enum GrantsOf {
    Object(ObjectGrants),
    Role(RoleGrants),
}

/// The Show Grants dialog: read-only, its answer landing only while it
/// still names the same request.
pub(crate) struct GrantsView {
    pub(crate) profile: String,
    pub(crate) request: u64,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) data: Loaded<GrantsOf>,
    /// Whether its rows are roles, an object's grants, which have members to
    /// list; a role's own grants are rows of relations.
    pub(crate) of_object: bool,
    /// Roles with members list them underneath.
    pub(crate) with_members: bool,
    pub(crate) filter: Entity<InputState>,
}

/// The names in a comma-separated field, blanks dropped.
fn names_in(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// A date the server reads as one, `YYYY-MM-DD`, or the field's complaint.
fn date_in(text: &str) -> Result<Option<String>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = text.split('-').collect();
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        [year, month, day] if digits(year, 4) && digits(month, 2) && digits(day, 2) => {
            Ok(Some(text.to_string()))
        }
        _ => Err("Password expires takes a date as YYYY-MM-DD.".into()),
    }
}

/// A connection limit, or the field's complaint.
fn limit_in(text: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    // An int4 on the server: a larger number would pass here and fail there.
    match text.parse::<i32>() {
        Ok(limit) if limit >= 0 => Ok(Some(limit.into())),
        _ => Err("Connection limit takes a whole number, or nothing for no limit.".into()),
    }
}

impl Workspace {
    /// What the connections tree shows of `profile`'s roles: nothing where
    /// its target manages none or it has not loaded.
    pub(crate) fn roles_listing(profile: &Profile) -> Option<RolesListing> {
        if !profile.config.target().manages_roles() {
            return None;
        }
        Some(match &profile.roles {
            RolesState::Unloaded => RolesListing::Unloaded,
            RolesState::Loading => RolesListing::Loading,
            RolesState::Failed(message) => RolesListing::Failed(message.clone()),
            RolesState::Loaded(roles) => RolesListing::Loaded(
                crate::users_view::ordered(roles)
                    .into_iter()
                    .map(|role| (role.name.clone(), crate::users_view::is_login(role)))
                    .collect(),
            ),
        })
    }

    /// Read `id`'s roles again, on a side connection, and hand them to the
    /// tree and the tab.
    pub(crate) fn load_roles(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        if !profile.config.target().manages_roles() {
            return;
        }
        let Some(connection) = profile.connection() else {
            return;
        };
        profile.roles = RolesState::Loading;
        profile.roles_request += 1;
        let request = profile.roles_request;
        let generation = profile.generation;
        let id = id.to_string();
        let side = self.roles_side(&id);
        let read = spawn_blocking(cx.background_executor(), move || {
            on_side(side, &connection, generation, |side| side.roles())
        });
        if let Some(view) = self.users_view_of(&id) {
            view.update(cx, |view, cx| view.set_roles(Loaded::Loading, cx));
        }
        self.rebuild_connections(cx);
        cx.spawn(async move |workspace, cx| {
            let result = read.await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace
                    .issued_to(&id, generation)
                    .filter(|profile| profile.roles_request == request)
                else {
                    return;
                };
                let loaded = match result {
                    Ok(roles) => {
                        let roles = Arc::new(roles);
                        profile.roles = RolesState::Loaded(roles.clone());
                        Loaded::Ready(roles)
                    }
                    Err(error) => {
                        profile.roles = RolesState::Failed(error.message.clone());
                        Loaded::Failed(error.message)
                    }
                };
                if let Some(view) = workspace.users_view_of(&id) {
                    view.update(cx, |view, cx| view.set_roles(loaded, cx));
                }
                workspace.rebuild_connections(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The kept session of `id`'s Users and roles tab, if it has one open.
    fn roles_side(&self, id: &str) -> Option<Arc<RolesSide>> {
        let profile = self.profiles.iter().find(|profile| profile.id == id)?;
        (profile.session.objects.iter()).find_map(|tab| match &tab.body {
            ObjectBody::Users { side, .. } => Some(side.clone()),
            _ => None,
        })
    }

    /// `id`'s Users and roles tab, if it has one open.
    fn users_view_of(&self, id: &str) -> Option<Entity<UsersView>> {
        let profile = self.profiles.iter().find(|profile| profile.id == id)?;
        profile
            .session
            .objects
            .iter()
            .find_map(|tab| match &tab.body {
                ObjectBody::Users { view, .. } => Some(view.clone()),
                _ => None,
            })
    }

    fn users_tab_of(&self, id: &str) -> Option<u64> {
        let profile = self.profiles.iter().find(|profile| profile.id == id)?;
        (profile.session.objects.iter())
            .find(|tab| matches!(tab.body, ObjectBody::Users { .. }))
            .map(|tab| tab.id)
    }

    /// Bring `id`'s Users and roles tab to the front, opening it if it has
    /// none, with `select` picked.
    pub(crate) fn open_users(
        &mut self,
        id: &str,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.make_front(id, cx) {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        if !profile.config.target().manages_roles() {
            return;
        }
        let tab = match self.users_tab_of(id) {
            Some(tab) => tab,
            None => {
                let server = profile
                    .active_database()
                    .unwrap_or_else(|| profile.name.clone());
                let view = cx.new(|_| UsersView::new(server));
                let profile_id = id.to_string();
                cx.subscribe_in(&view, window, move |workspace, _, event, window, cx| {
                    workspace.users_event(&profile_id, event, window, cx)
                })
                .detach();
                let results = result_grid::new_grid(window, cx);
                let Some(profile) = self.profile_mut() else {
                    return;
                };
                let tab = profile.session.next_object_id;
                profile.session.next_object_id += 1;
                profile.session.objects.push(ObjectTab {
                    id: tab,
                    database: None,
                    schema: String::new(),
                    name: crate::sidebar::USERS_AND_ROLES.to_string(),
                    kind: None,
                    body: ObjectBody::Users {
                        view,
                        query: QueryState::Idle,
                        results,
                        side: Arc::default(),
                    },
                });
                profile.session.place_last(session::TabKey::Object(tab));
                tab
            }
        };
        self.activate_tab(Tab::Object(tab), window, cx);
        if let (Some(name), Some(view)) = (select, self.users_view_of(id)) {
            view.update(cx, |view, cx| view.select(&name, cx));
        }
    }

    /// The Users and roles tab came to the front: read the roles if nothing
    /// has, or hand over what was read.
    pub(crate) fn show_users(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let id = profile.id.clone();
        match profile.roles.clone() {
            RolesState::Unloaded | RolesState::Failed(_) => self.load_roles(&id, cx),
            RolesState::Loading => {}
            RolesState::Loaded(roles) => {
                if let Some(view) = self.users_view_of(&id)
                    && view
                        .read(cx)
                        .roles()
                        .is_none_or(|shown| !Arc::ptr_eq(shown, &roles))
                {
                    view.update(cx, |view, cx| view.set_roles(Loaded::Ready(roles), cx));
                }
            }
        }
    }

    fn users_event(
        &mut self,
        id: &str,
        event: &UsersEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            UsersEvent::Select(name, request) => self.read_reach(id, name.clone(), *request, cx),
            UsersEvent::Reports(request) => self.read_reports(id, *request, cx),
            UsersEvent::NewRole { login } => {
                self.open_role_form(id, RoleFormKind::New { login: *login }, window, cx)
            }
            UsersEvent::Edit(name) => {
                if let Some(role) = self.role_of(id, name) {
                    self.open_role_form(id, RoleFormKind::Edit(role), window, cx);
                }
            }
            UsersEvent::ResetPassword(name) => {
                self.open_role_form(id, RoleFormKind::Password(name.clone()), window, cx)
            }
            UsersEvent::ShowGrants(name) => self.show_role_grants(id, name.clone(), window, cx),
            UsersEvent::SetLogin(name, login) => {
                let title = if *login {
                    "Allow login"
                } else {
                    "Deactivate login"
                };
                self.review_from_tab(id, title, roles::set_login(name, *login), window, cx);
            }
            UsersEvent::Export => self.export_reports(id, cx),
        }
    }

    fn role_of(&self, id: &str, name: &str) -> Option<Role> {
        let profile = self.profiles.iter().find(|profile| profile.id == id)?;
        match &profile.roles {
            RolesState::Loaded(roles) => roles.role(name).cloned(),
            _ => None,
        }
    }

    fn read_reach(&mut self, id: &str, name: String, request: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let (id, generation) = (id.to_string(), profile.generation);
        let side = self.roles_side(&id);
        let read = spawn_blocking(cx.background_executor(), move || {
            on_side(side, &connection, generation, |side| side.role_reach(&name))
        });
        cx.spawn(async move |workspace, cx| {
            let result = read.await;
            _ = workspace.update(cx, |workspace, cx| {
                if workspace.issued_to(&id, generation).is_none() {
                    return;
                }
                let reach = match result {
                    Ok(reach) => Loaded::Ready(reach),
                    Err(error) => Loaded::Failed(error.message),
                };
                if let Some(view) = workspace.users_view_of(&id) {
                    view.update(cx, |view, cx| view.set_reach(request, reach, cx));
                }
            });
        })
        .detach();
    }

    fn read_reports(&mut self, id: &str, request: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let (Some(connection), RolesState::Loaded(roles)) =
            (profile.connection(), profile.roles.clone())
        else {
            // The reports are read against the roles; with none read, say so
            // rather than wait on a read nothing started.
            if let Some(view) = self.users_view_of(id) {
                view.update(cx, |view, cx| {
                    view.set_reports(
                        request,
                        Loaded::Failed("the roles have not been read".into()),
                        cx,
                    )
                });
            }
            return;
        };
        let (id, generation) = (id.to_string(), profile.generation);
        let side = self.roles_side(&id);
        let read = spawn_blocking(cx.background_executor(), move || {
            on_side(side, &connection, generation, |side| {
                side.role_reports(&roles)
            })
        });
        cx.spawn(async move |workspace, cx| {
            let result = read.await;
            _ = workspace.update(cx, |workspace, cx| {
                if workspace.issued_to(&id, generation).is_none() {
                    return;
                }
                let reports = match result {
                    Ok(reports) => Loaded::Ready(reports),
                    Err(error) => Loaded::Failed(error.message),
                };
                if let Some(view) = workspace.users_view_of(&id) {
                    view.update(cx, |view, cx| view.set_reports(request, reports, cx));
                }
            });
        })
        .detach();
    }

    /// Write the Reports to a CSV file the user picks.
    fn export_reports(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(view) = self.users_view_of(id) else {
            return;
        };
        let Some(csv) = view.read(cx).reports().map(reports_csv) else {
            return;
        };
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let (id, generation) = (id.to_string(), profile.generation);
        let chosen = choose_path("users-and-roles-reports", "csv", cx);
        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            let written = spawn_blocking(cx.background_executor(), move || {
                write_replacing(&path, |file| {
                    file.write_all(csv.as_bytes())
                        .map_err(could_not_write(&path))
                })
                .map(|()| format!("Exported the reports to {}.", path.display()))
                .map_err(|ended| ended.notice(&path))
            })
            .await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                match written {
                    Ok(message) => {
                        profile.session.notice = Some(message.clone());
                        workspace.clear_notice_later(id, generation, message, cx);
                    }
                    Err(message) => profile.session.notice = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn open_role_form(
        &mut self,
        id: &str,
        kind: RoleFormKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.users_tab_of(id).is_none() {
            return;
        }
        let (limit, expires) = match &kind {
            RoleFormKind::Edit(role) => (
                role.connection_limit
                    .map(|limit| limit.to_string())
                    .unwrap_or_default(),
                role.password_expires.clone().unwrap_or_default(),
            ),
            _ => Default::default(),
        };
        let input = |value: String,
                     placeholder: &'static str,
                     masked: bool,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
                    .masked(masked)
            })
        };
        let form = RoleForm {
            profile: id.to_string(),
            name: input(
                String::new(),
                "Name, as the server will spell it",
                false,
                window,
                cx,
            ),
            password: input(String::new(), "Optional", true, window, cx),
            confirm: input(String::new(), "Type it again", true, window, cx),
            limit: input(limit, "No limit", false, window, cx),
            expires: input(expires, "Never, or YYYY-MM-DD", false, window, cx),
            member_of: input(String::new(), "Roles, comma-separated", false, window, cx),
            kind,
            error: None,
        };
        // Enter in any field is Review…, as a form's default button is: the
        // dialog's ring passes over a form with fields of its own.
        for input in [
            &form.name,
            &form.password,
            &form.confirm,
            &form.limit,
            &form.expires,
            &form.member_of,
        ] {
            cx.subscribe_in(
                input,
                window,
                |workspace, _, event: &gpui_component::input::InputEvent, window, cx| {
                    if matches!(event, gpui_component::input::InputEvent::PressEnter { .. }) {
                        workspace.submit_role_form(window, cx);
                    }
                },
            )
            .detach();
        }
        let first = match form.kind {
            RoleFormKind::New { .. } => form.name.clone(),
            RoleFormKind::Edit(_) => form.limit.clone(),
            RoleFormKind::Password(_) => form.password.clone(),
        };
        self.role_form = Some(form);
        // The dialog layer takes the keyboard on the frame that raises the
        // form; the first field takes it back on the frame after.
        cx.on_next_frame(window, move |_, window, cx| {
            cx.on_next_frame(window, move |workspace, window, cx| {
                if workspace.role_form.is_some() {
                    first.update(cx, |input, cx| input.focus(window, cx));
                }
            });
        });
        cx.notify();
    }

    /// Close the role form, its fields with it, and hand the keyboard back.
    pub(crate) fn close_role_form(&mut self, cx: &mut Context<Self>) -> bool {
        // Drawn only over its own connection, so only there does `escape`
        // reach it.
        let front = self.profile().map(|profile| profile.id.clone());
        if self.role_form.as_ref().map(|form| Some(&form.profile)) != Some(front.as_ref()) {
            return false;
        }
        self.role_form = None;
        self.refocus_front();
        cx.notify();
        true
    }

    /// The form's statement, or what is wrong with what was typed.
    fn role_form_statement(
        &self,
        form: &RoleForm,
        cx: &App,
    ) -> Result<(String, &'static str), String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let password = value(&form.password);
        if password != value(&form.confirm) {
            return Err("The two passwords differ.".into());
        }
        let password = (!password.is_empty()).then_some(password);
        match &form.kind {
            RoleFormKind::New { login } => {
                let name = value(&form.name).trim().to_string();
                if name.is_empty() {
                    return Err("A role needs a name.".into());
                }
                let member_of = names_in(&value(&form.member_of));
                self.refused_membership(&form.profile, &member_of)?;
                let role = NewRole {
                    name,
                    login: *login,
                    password: password.filter(|_| *login),
                    connection_limit: limit_in(&value(&form.limit))?,
                    password_expires: date_in(&value(&form.expires))?,
                    member_of,
                };
                let title = if *login { "Create user" } else { "Create role" };
                Ok((roles::create_role(&role), title))
            }
            RoleFormKind::Edit(before) => {
                let edit = RoleEdit {
                    name: before.name.clone(),
                    connection_limit: limit_in(&value(&form.limit))?,
                    password_expires: date_in(&value(&form.expires))?,
                };
                if roles::clears_limit(before, &edit) {
                    return Err("A connection limit can be changed here, not removed.".into());
                }
                roles::edit_role(before, &edit)
                    .map(|sql| (sql, "Edit role"))
                    .ok_or_else(|| "Nothing was changed.".into())
            }
            RoleFormKind::Password(name) => match password {
                Some(password) => Ok((roles::reset_password(name, &password), "Reset password")),
                None => Err("Type the new password.".into()),
            },
        }
    }

    /// Refuses a membership that is a way into superuser, by what the roles
    /// read say of each: one the gate would not know by name is a
    /// superuser of another name, or a role that belongs to one.
    fn refused_membership(&self, id: &str, member_of: &[String]) -> Result<(), String> {
        if member_of.is_empty() {
            return Ok(());
        }
        let profile = self.profiles.iter().find(|profile| profile.id == id);
        let Some(RolesState::Loaded(roles)) = profile.map(|profile| &profile.roles) else {
            return Err("Member of waits on the roles being read.".into());
        };
        match member_of
            .iter()
            .find_map(|name| roles.refuses_membership(name))
        {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// The form's Review: its statement goes up for review, and the form
    /// closes once it has. A change refused on the way keeps the form, what
    /// was typed in it included, and says why on it.
    pub(crate) fn submit_role_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.role_form.as_ref() else {
            return;
        };
        let id = form.profile.clone();
        let reviewed = match self.role_form_statement(form, cx) {
            Ok((sql, title)) => self.review_role_change(&id, title, sql, window, cx),
            Err(message) => Err(message),
        };
        match reviewed {
            Ok(()) => self.role_form = None,
            Err(message) => {
                if let Some(form) = self.role_form.as_mut() {
                    form.error = Some(message);
                }
            }
        }
        cx.notify();
    }

    /// Put a role change up for review over `id`'s Users and roles tab, once
    /// the gate has read it as one; or why it was not.
    pub(super) fn review_role_change(
        &mut self,
        id: &str,
        title: &'static str,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(tab) = self.users_tab_of(id) else {
            return Err("The Users and roles tab is closed.".into());
        };
        // One change at a time: the slot holds the running one's Cancel and
        // its landing, which a second would overwrite.
        let running = (self.profiles.iter())
            .find(|profile| profile.id == id)
            .is_some_and(|profile| profile.session.running(Tab::Object(tab)));
        if running {
            return Err(ROLE_CHANGE_RUNNING.into());
        }
        if !self.make_front(id, cx) {
            return Err("The connection is not open.".into());
        }
        // The gate every statement this tab writes passes (`AGENTS.md` rule
        // 2): failing it is a bug in dbdelve, not something the user typed.
        if !sql::is_generated_role_change(self.engine(), &sql) {
            return Err(
                "dbdelve refused to run a statement it wrote itself: it is not a role change."
                    .into(),
            );
        }
        self.activate_tab(Tab::Object(tab), window, cx);
        if !self.review_slot_free(Tab::Object(tab), true, window, cx) {
            return Err("Run or close the statement up for review first.".into());
        }
        if let Some(profile) = self.profile_mut() {
            // What an earlier change left in the slot is not this one's.
            if let Some((query, _)) = profile.session.slot(Tab::Object(tab)) {
                *query = QueryState::Idle;
            }
            profile.session.apply_review = Some(ApplyReview {
                tab: Tab::Object(tab),
                title,
                notes: vec![
                    "A role change needs the Full connection mode. A password is sent as its \
                     SCRAM-SHA-256 hash, never as typed."
                        .into(),
                ],
                sql,
                error: None,
                edits: false,
                verify: None,
                ran_on: None,
            });
            cx.notify();
        }
        Ok(())
    }

    /// A change asked for from the tab rather than the form, said in the
    /// status bar when it is refused.
    fn review_from_tab(
        &mut self,
        id: &str,
        title: &'static str,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(message) = self.review_role_change(id, title, sql, window, cx) {
            self.note(message, cx);
        }
    }

    /// A role change landed: read the roles again.
    pub(crate) fn refresh_roles(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.profile().map(|profile| profile.id.clone()) {
            self.load_roles(&id, cx);
        }
    }

    /// Show Grants on `node`, a relation or sequence, a `kind` under the
    /// dialog's title.
    pub(crate) fn show_object_grants(
        &mut self,
        node: &crate::sidebar::SelectedNode,
        kind: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(schema) = node.schema.clone() else {
            return;
        };
        let (profile, database, name) = (
            node.profile.as_str(),
            node.database.clone(),
            node.name.clone(),
        );
        if !self.make_front(profile, cx) {
            return;
        }
        let Some((profile_id, generation, routed)) = self.route_in(profile, database.clone())
        else {
            return;
        };
        let shown_database = self
            .elsewhere(database)
            .or_else(|| self.profile().and_then(Profile::active_database));
        let subtitle = [Some(kind.to_string()), shown_database, Some(schema.clone())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let (asked_schema, asked) = (schema.clone(), name.clone());
        let task = self.run_routed(
            &profile_id,
            generation,
            routed,
            move |connection| connection.apart()?.object_grants(&asked_schema, &asked),
            cx,
        );
        let request = self.open_grants_view(
            &profile_id,
            format!("Grants on {name}"),
            subtitle,
            true,
            window,
            cx,
        );
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.land_grants(
                    &profile_id,
                    generation,
                    request,
                    result.map(GrantsOf::Object),
                    cx,
                )
            });
        })
        .detach();
    }

    /// Show Grants of one role: every relation it holds anything on.
    fn show_role_grants(
        &mut self,
        id: &str,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let generation = profile.generation;
        let database = profile
            .active_database()
            .unwrap_or_else(|| profile.name.clone());
        let asked = name.clone();
        let side = self.roles_side(id);
        let read = spawn_blocking(cx.background_executor(), move || {
            on_side(side, &connection, generation, |side| {
                side.role_grants(&asked)
            })
        });
        let kind = match self.role_of(id, &name) {
            Some(role) if crate::users_view::is_login(&role) => "login",
            _ => "role",
        };
        let request = self.open_grants_view(
            id,
            format!("Grants of {name}"),
            format!("{kind} · {database}"),
            false,
            window,
            cx,
        );
        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let result = read.await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.land_grants(&id, generation, request, result.map(GrantsOf::Role), cx)
            });
        })
        .detach();
    }

    pub(super) fn open_grants_view(
        &mut self,
        profile: &str,
        title: String,
        subtitle: String,
        of_object: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.clear_notice();
        self.last_grants_request += 1;
        let request = self.last_grants_request;
        let placeholder = if of_object {
            "Filter roles…"
        } else {
            "Filter objects…"
        };
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        cx.observe(&filter, |_, _, cx| cx.notify()).detach();
        self.grants_view = Some(GrantsView {
            profile: profile.to_string(),
            request,
            title,
            subtitle,
            data: Loaded::Loading,
            of_object,
            with_members: of_object,
            filter,
        });
        cx.notify();
        request
    }

    pub(super) fn land_grants(
        &mut self,
        id: &str,
        generation: u64,
        request: u64,
        result: Result<GrantsOf, DbError>,
        cx: &mut Context<Self>,
    ) {
        if self.issued_to(id, generation).is_none() {
            return;
        }
        let Some(view) = self
            .grants_view
            .as_mut()
            .filter(|view| view.request == request)
        else {
            return;
        };
        if let Ok(GrantsOf::Object(grants)) = &result {
            view.subtitle = format!("{} · owner {}", view.subtitle, grants.owner);
        }
        view.data = match result {
            Ok(grants) => Loaded::Ready(grants),
            Err(error) => Loaded::Failed(format!("Could not read the grants: {}", error.message)),
        };
        cx.notify();
    }

    /// The Show Grants dialog, when it is the front connection's.
    pub(crate) fn grants_view_in_front(&self) -> Option<&GrantsView> {
        let profile = self.profile()?;
        self.grants_view
            .as_ref()
            .filter(|view| view.profile == profile.id)
    }

    pub(crate) fn close_grants_view(&mut self, cx: &mut Context<Self>) -> bool {
        if self.grants_view_in_front().is_none() {
            return false;
        }
        self.grants_view = None;
        self.refocus_front();
        cx.notify();
        true
    }

    pub(crate) fn set_grants_members(&mut self, with_members: bool, cx: &mut Context<Self>) {
        if let Some(view) = self.grants_view.as_mut() {
            view.with_members = with_members;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tabs_reads_share_one_side_session_until_the_profile_connects_again() {
        use crate::db::{CancelToken, ConnectionConfig, Fetch};
        let path =
            std::env::temp_dir().join(format!("dbdelve-roles-side-{}.db", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let connection = Connection::open(ConnectionConfig::Sqlite {
            path: path.display().to_string(),
            statement_timeout: 0,
        })
        .unwrap();
        let run = |session: &Connection, sql: &str| {
            session
                .query(sql, &CancelToken::default(), Fetch::default())
                .map(|_| ())
        };
        let side = RolesSide::default();
        assert!(!side.is_open());
        // A temporary table is the session's own: seen again only on it.
        side.read(&connection, 1, |session| {
            run(session, "CREATE TEMP TABLE mark (x)")
        })
        .unwrap();
        assert!(side.is_open());
        side.read(&connection, 1, |session| {
            run(session, "SELECT * FROM temp.mark")
        })
        .expect("the second read is on the session the first opened");
        assert!(
            side.read(&connection, 2, |session| run(
                session,
                "SELECT * FROM temp.mark"
            ))
            .is_err(),
            "a later connect's read opens a session of its own"
        );
        assert!(run(&connection, "SELECT * FROM temp.mark").is_err());
        drop((side, connection));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn form_fields_read_as_the_server_would_or_say_why_not() {
        assert_eq!(names_in(" reporting, ,app_rw "), ["reporting", "app_rw"]);
        assert_eq!(date_in("2027-01-31"), Ok(Some("2027-01-31".into())));
        assert_eq!(date_in(""), Ok(None));
        assert!(date_in("31/01/2027").is_err());
        assert_eq!(limit_in("5"), Ok(Some(5)));
        assert_eq!(limit_in(" "), Ok(None));
        assert!(limit_in("-1").is_err());
        assert_eq!(limit_in("2147483647"), Ok(Some(2147483647)));
        assert!(limit_in("99999999999").is_err(), "past an int4");
    }
}
