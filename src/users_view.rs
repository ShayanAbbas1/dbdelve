//! A server's Users and roles tab: every login and role in a list, the one
//! picked in a panel beside it, and the Reports. What it shows is what the
//! workspace read and handed it; what it asks for goes back as an event, and
//! no statement is written or run from here.

use std::sync::Arc;

use gpui::{
    AnyElement, Context, EventEmitter, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder, px, relative,
};
use gpui_component::{Disableable as _, Icon, button::Button};

use crate::actions::{UsersListFirst, UsersListLast, UsersListNext, UsersListPrevious};
use crate::db::roles::{Reach, Reports, Role, Roles};
use crate::icons::icon;
use crate::theme::{ConnectionColor, Theme, fonts, layout, theme};
use crate::ui::{self, Control, Tone};

/// What has been read of something, for a part of the tab that waits on it.
#[derive(Clone)]
pub enum Loaded<T> {
    Loading,
    Failed(String),
    Ready(T),
}

/// What the tab asks the workspace to do.
pub enum UsersEvent {
    /// A row was picked: read what it can reach. The answer lands only
    /// while it carries the request the view last asked under.
    Select(String, u64),
    /// The Reports were opened: read them, under a request as `Select`'s.
    Reports(u64),
    NewRole {
        login: bool,
    },
    Edit(String),
    ResetPassword(String),
    ShowGrants(String),
    /// Turn a login off, or back on.
    SetLogin(String, bool),
    /// Write the Reports to a CSV file.
    Export,
}

pub struct UsersView {
    /// The connection and its database, under the title.
    server: SharedString,
    roles: Loaded<Arc<Roles>>,
    /// The list's rows, built once when the roles land rather than on every
    /// frame: each role, in the order drawn, with what it is a member of.
    listing: Vec<Listed>,
    selected: Option<String>,
    reach: Option<Loaded<Reach>>,
    /// The last read of what a row reaches and of the Reports: an older
    /// answer, which can land after a newer one, is dropped. By request and
    /// not by name, since the same row picked again is a newer read.
    reach_request: u64,
    reports_request: u64,
    reports_shown: bool,
    reports: Option<Loaded<Reports>>,
    /// What the list's keys are bound to (`UsersList`): focused, the arrows
    /// move the pick; a form or an input holding focus leaves them alone.
    focus: FocusHandle,
    /// The list's scroll, to bring a row the keys picked into view.
    scroll: ScrollHandle,
}

impl EventEmitter<UsersEvent> for UsersView {}

/// The status a row's pill says, and the hue it says it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    Deactivated,
    NeverLoggedIn,
    Role,
}

impl Status {
    pub fn of(role: &Role, roles: &Roles) -> Self {
        if role.deactivated() {
            Status::Deactivated
        } else if !role.login {
            Status::Role
        } else if roles.records_logins && role.last_login.is_none() {
            Status::NeverLoggedIn
        } else {
            Status::Active
        }
    }

    fn label(self) -> &'static str {
        match self {
            Status::Active => "Active",
            Status::Deactivated => "Deactivated",
            Status::NeverLoggedIn => "Never logged in",
            Status::Role => "Role",
        }
    }
}

/// Whether a row is a login, one that can log in or was turned off, rather
/// than a role made to be granted.
pub fn is_login(role: &Role) -> bool {
    role.login || role.deactivated()
}

/// Logins first, then roles, each in name order: what the list and the
/// sidebar both show.
pub fn ordered(roles: &Roles) -> Vec<&Role> {
    let mut ordered: Vec<&Role> = roles.roles.iter().collect();
    ordered.sort_by(|a, b| is_login(b).cmp(&is_login(a)).then(a.name.cmp(&b.name)));
    ordered
}

/// The panel's plain-language answer to what the role can reach in the
/// connection's database: its grants on relations, sequences and columns,
/// in schemas it may use, what it owns, and the predefined roles it holds.
/// It ends on what it read, relations, and claims nothing of the rest: a
/// routine's `EXECUTE` or a schema's `CREATE` is not counted.
pub fn reach_sentence(role: &Role, reach: &Reach) -> String {
    if role.superuser {
        return "Everything: a superuser bypasses every grant.".into();
    }
    // One phrase per schema, count and route, its privileges joined.
    let mut phrases: Vec<(String, u64, Vec<String>, Vec<String>)> = Vec::new();
    for entry in &reach.entries {
        let privilege = crate::grants::column_label(&entry.privilege);
        match phrases.iter_mut().find(|(schema, count, _, through)| {
            *schema == entry.schema && *count == entry.relations && *through == entry.through
        }) {
            Some((_, _, privileges, _)) => privileges.push(privilege),
            None => phrases.push((
                entry.schema.clone(),
                entry.relations,
                vec![privilege],
                entry.through.clone(),
            )),
        }
    }
    let mut sentences: Vec<String> = phrases
        .into_iter()
        .map(|(schema, count, privileges, through)| {
            let privileges = match privileges.as_slice() {
                [only] => only.clone(),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                [] => String::new(),
            };
            let relations = if count == 1 { "relation" } else { "relations" };
            let route = match through.as_slice() {
                [] => String::new(),
                through => format!(", through {}", through.join(", ")),
            };
            format!(
                "{privileges} on {count} {relations} in {}.{schema}{route}.",
                reach.database
            )
        })
        .collect();
    if reach.owns > 0 {
        let relations = if reach.owns == 1 {
            "relation"
        } else {
            "relations"
        };
        sentences.push(format!("Owns {} {relations}.", reach.owns));
    }
    // A predefined role reaches past every grant above; naming it is the
    // only honest summary of what it adds.
    if !reach.predefined.is_empty() {
        sentences.push(format!("Holds {}.", reach.predefined.join(", ")));
    }
    match sentences.is_empty() {
        true => format!("No grant on a relation in {}.", reach.database),
        false => {
            sentences.push("No other grant on a relation.".into());
            sentences.join(" ")
        }
    }
}

/// How a key moves the pick through the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Previous,
    Next,
    First,
    Last,
}

/// The row a key lands on in a list of `len`, from the one picked, `None`
/// for an empty list. Up and Down wrap, as the palette's do: a list this
/// short is faster to cross from the other end than by holding a key; with
/// nothing picked they start from the end they point away from. Home and End
/// go to the ends.
fn stepped(len: usize, from: Option<usize>, step: Step) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match (step, from) {
        (Step::First, _) | (Step::Next, None) => 0,
        (Step::Last, _) | (Step::Previous, None) => last,
        (Step::Next, Some(at)) => (at + 1) % len,
        (Step::Previous, Some(at)) => (at + last) % len,
    })
}

/// One row of the list, as [`listing`] builds it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listed {
    role: Role,
    /// The roles it belongs to, in name order and joined, or `—`.
    member_of: String,
}

/// The list's rows: [`ordered`], each with what it is a member of, the
/// memberships gone through once rather than once per row.
fn listing(roles: &Roles) -> Vec<Listed> {
    let mut of: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (member, role) in &roles.memberships {
        of.entry(member.as_str()).or_default().push(role.as_str());
    }
    ordered(roles)
        .into_iter()
        .map(|role| {
            let mut member_of = of.remove(role.name.as_str()).unwrap_or_default();
            member_of.sort_unstable();
            Listed {
                role: role.clone(),
                member_of: match member_of.is_empty() {
                    true => "—".to_string(),
                    false => member_of.join(", "),
                },
            }
        })
        .collect()
}

/// The two letters of the avatar: the first of each of the first two words
/// of the name, or its first two letters.
fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let letters: String = match words.as_slice() {
        [first, second, ..] => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
        [only] => only.chars().take(2).collect(),
        [] => name.chars().take(2).collect(),
    };
    letters.to_uppercase()
}

impl UsersView {
    pub fn new(server: String, cx: &mut Context<Self>) -> Self {
        Self {
            server: server.into(),
            roles: Loaded::Loading,
            listing: Vec::new(),
            selected: None,
            reach: None,
            reach_request: 0,
            reports_request: 0,
            reports_shown: false,
            reports: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Move the pick by `step`, as a click on that row would, and bring the
    /// row into view.
    fn step(&mut self, step: Step, cx: &mut Context<Self>) {
        let at = self.selected.as_deref().and_then(|name| {
            self.listing
                .iter()
                .position(|listed| listed.role.name == name)
        });
        let Some(to) = stepped(self.listing.len(), at, step) else {
            return;
        };
        // The header is the list's first child.
        self.scroll.scroll_to_item(to + 1);
        let name = self.listing[to].role.name.clone();
        self.select(&name, cx);
    }

    pub fn reports(&self) -> Option<&Reports> {
        match &self.reports {
            Some(Loaded::Ready(reports)) => Some(reports),
            _ => None,
        }
    }

    pub fn roles(&self) -> Option<&Arc<Roles>> {
        match &self.roles {
            Loaded::Ready(roles) => Some(roles),
            _ => None,
        }
    }

    /// What was read of the roles. A pick that is gone is let go; with none,
    /// the first row is picked, as the list draws it.
    pub fn set_roles(&mut self, roles: Loaded<Arc<Roles>>, cx: &mut Context<Self>) {
        if let Loaded::Ready(read) = &roles {
            let kept = (self.selected.as_ref()).filter(|name| read.role(name).is_some());
            if kept.is_none() {
                self.selected = ordered(read).first().map(|role| role.name.clone());
                self.reach = None;
            }
            // What a role can reach may have changed with it.
            self.reports = None;
        }
        self.listing = match &roles {
            Loaded::Ready(read) => listing(read),
            _ => Vec::new(),
        };
        // Each read opens a side connection: asked once the roles have landed,
        // not again for the Loading before them.
        let ready = matches!(roles, Loaded::Ready(_));
        self.roles = roles;
        if let Some(name) = self.selected.clone().filter(|_| ready) {
            self.ask_reach(name, cx);
        }
        if self.reports_shown && ready {
            self.ask_reports(cx);
        }
        cx.notify();
    }

    /// The pick, without asking for what it reaches: a sidebar row naming it.
    pub fn select(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(name) && !self.reports_shown {
            return;
        }
        self.reports_shown = false;
        self.selected = Some(name.to_string());
        self.ask_reach(name.to_string(), cx);
        cx.notify();
    }

    fn ask_reach(&mut self, name: String, cx: &mut Context<Self>) {
        self.reach_request += 1;
        self.reach = Some(Loaded::Loading);
        cx.emit(UsersEvent::Select(name, self.reach_request));
    }

    fn ask_reports(&mut self, cx: &mut Context<Self>) {
        self.reports_request += 1;
        self.reports = Some(Loaded::Loading);
        cx.emit(UsersEvent::Reports(self.reports_request));
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn reach(&self) -> Option<&Loaded<Reach>> {
        self.reach.as_ref()
    }

    /// What the pick reaches, if `request` is still the last one asked.
    pub fn set_reach(&mut self, request: u64, reach: Loaded<Reach>, cx: &mut Context<Self>) {
        if request == self.reach_request {
            self.reach = Some(reach);
            cx.notify();
        }
    }

    /// The Reports, if `request` is still the last one asked.
    pub fn set_reports(&mut self, request: u64, reports: Loaded<Reports>, cx: &mut Context<Self>) {
        if request == self.reports_request {
            self.reports = Some(reports);
            cx.notify();
        }
    }

    fn show_reports(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.reports_shown == shown {
            return;
        }
        self.reports_shown = shown;
        if shown && !matches!(self.reports, Some(Loaded::Ready(_) | Loaded::Loading)) {
            self.ask_reports(cx);
        }
        cx.notify();
    }

    fn header(&self, t: Theme, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let users = cx.listener(|view, _, _, cx| view.show_reports(false, cx));
        let reports = cx.listener(|view, _, _, cx| view.show_reports(true, cx));
        let new_user = cx.listener(|_, _, _, cx| cx.emit(UsersEvent::NewRole { login: true }));
        let new_role = cx.listener(|_, _, _, cx| cx.emit(UsersEvent::NewRole { login: false }));
        let export = cx.listener(|_, _, _, cx| cx.emit(UsersEvent::Export));
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(18.))
            .py(px(14.))
            .border_b_1()
            .border_color(t.border)
            // Chrome: no fill of its own, so a glass theme's frost shows.
            .child(
                Icon::default()
                    .path(icon::USERS)
                    .size(px(16.))
                    .text_color(ConnectionColor::Purple.ink(t)),
            )
            .child(
                div()
                    .text_size(px(layout::chrome(15.)))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text)
                    .child(crate::sidebar::USERS_AND_ROLES),
            )
            .child(
                div()
                    .font_family(mono)
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child(format!("{} · server-wide", self.server)),
            )
            .child(
                ui::segmented(t)
                    .ml(px(14.))
                    .debug_selector(|| "users-view-switch".into())
                    .child(
                        ui::segmented_choice(
                            "users-tab-users",
                            crate::sidebar::USERS_AND_ROLES,
                            !self.reports_shown,
                            true,
                            t,
                        )
                        .on_click(users),
                    )
                    .child(
                        ui::segmented_choice(
                            "users-tab-reports",
                            "Reports",
                            self.reports_shown,
                            true,
                            t,
                        )
                        .on_click(reports),
                    ),
            )
            .child(div().flex_1())
            .when(self.reports_shown, |header| {
                header.child(
                    ui::button(
                        "users-export",
                        "Export CSV…",
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .disabled(self.reports().is_none())
                    .on_click(export),
                )
            })
            .child(
                ui::button(
                    "users-new-user",
                    "New User…",
                    Tone::Primary,
                    Control::Compact,
                    t,
                )
                .on_click(new_user),
            )
            .child(outline_button("users-new-role", "New Role…", t, t.text).on_click(new_role))
    }

    fn list(
        &self,
        roles: &Roles,
        t: Theme,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let head = |label: &'static str| {
            div()
                .text_size(px(layout::chrome(layout::TEXT_XS)))
                .text_color(t.text_muted)
                .font_weight(FontWeight::MEDIUM)
                .child(label.to_uppercase())
        };
        let columns = |row: gpui::Div| {
            row.flex()
                .items_center()
                .px(px(18.))
                .gap(px(layout::SPACE_SM))
        };
        // The widths the artboard draws at its own width, as shares of it.
        let widths = [0.24, 0.12, 0.24, 0.2, 0.2];
        let cell = |at: usize, child: AnyElement| {
            div()
                .w(relative(widths[at]))
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(child)
        };
        let header = columns(div().h(px(32.)))
            .child(cell(0, head("Name").into_any_element()))
            .child(cell(1, head("Kind").into_any_element()))
            .child(cell(2, head("Member of").into_any_element()))
            .child(cell(3, head("Last login").into_any_element()))
            .child(cell(4, head("Status").into_any_element()));
        let rows = self.listing.iter().map(|Listed { role, member_of }| {
            let status = Status::of(role, roles);
            let selected = self.selected.as_deref() == Some(role.name.as_str());
            let member_of = member_of.clone();
            let login = role.last_login.clone().unwrap_or_else(|| "—".into());
            let name = role.name.clone();
            let pick = cx.listener(move |view, _, window, cx| {
                // A click puts the keys on the list, whatever held them.
                view.focus.focus(window, cx);
                view.select(&name, cx)
            });
            columns(div())
                .id(SharedString::from(format!("users-row-{}", role.name)))
                .debug_selector({
                    let name = role.name.clone();
                    move || format!("users-row-{name}")
                })
                .h(px(38.))
                .border_t_1()
                .border_color(t.border)
                .when(selected, |row| row.bg(t.element_active))
                .hover(|row| row.bg(t.element_hover))
                .cursor_pointer()
                .on_click(pick)
                .text_size(px(layout::chrome(12.5)))
                .child(cell(
                    0,
                    div()
                        .font_family(mono.clone())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(match status {
                            Status::Deactivated => t.text_faint,
                            _ => t.text,
                        })
                        .child(role.name.clone())
                        .into_any_element(),
                ))
                .child(cell(
                    1,
                    div()
                        .text_color(t.text_muted)
                        .child(if is_login(role) { "Login" } else { "Role" })
                        .into_any_element(),
                ))
                .child(cell(
                    2,
                    div()
                        .font_family(mono.clone())
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text)
                        .child(member_of)
                        .into_any_element(),
                ))
                .child(cell(
                    3,
                    div()
                        .text_color(if role.last_login.is_some() {
                            t.text
                        } else {
                            t.text_faint
                        })
                        .child(login)
                        .into_any_element(),
                ))
                .child(cell(4, status_pill(status, t).into_any_element()))
        });
        div()
            .id("users-list")
            .key_context("UsersList")
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|view, _: &UsersListPrevious, _, cx| view.step(Step::Previous, cx)),
            )
            .on_action(cx.listener(|view, _: &UsersListNext, _, cx| view.step(Step::Next, cx)))
            .on_action(cx.listener(|view, _: &UsersListFirst, _, cx| view.step(Step::First, cx)))
            .on_action(cx.listener(|view, _: &UsersListLast, _, cx| view.step(Step::Last, cx)))
            .flex_1()
            .min_w_0()
            .py(px(layout::SPACE_SM))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(header)
            .children(rows)
            .into_any_element()
    }

    fn detail(
        &self,
        roles: &Roles,
        t: Theme,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = div()
            .w(px(320.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_MD))
            .px(px(18.))
            .py(px(layout::SPACE_LG))
            .border_l_1()
            .border_color(t.border)
            .bg(t.panel_glass())
            .debug_selector(|| "users-detail".into());
        let Some(role) = self.selected.as_deref().and_then(|name| roles.role(name)) else {
            return panel
                .child(
                    div()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text_muted)
                        .child("No login or role picked."),
                )
                .into_any_element();
        };
        let login = is_login(role);
        let member_of = roles.member_of(&role.name);
        let members = roles.members(&role.name);
        let subtitle = match (login, member_of.as_slice(), members.len()) {
            (true, [], _) => "Login".to_string(),
            (true, of, _) => format!("Login · member of {}", of.join(", ")),
            (false, _, 0) => "Role · no members".to_string(),
            (false, _, 1) => "Role · 1 member".to_string(),
            (false, _, n) => format!("Role · {n} members"),
        };
        let yes_no = |yes: bool| if yes { "Yes" } else { "No" }.to_string();
        let facts = [
            (
                "Password expires",
                role.password_expires
                    .clone()
                    .unwrap_or_else(|| "Never".into()),
                true,
            ),
            (
                "Connection limit",
                role.connection_limit
                    .map_or_else(|| "No limit".into(), |limit| limit.to_string()),
                true,
            ),
            ("Can log in", yes_no(role.login), false),
            ("Superuser", yes_no(role.superuser), false),
        ];
        let fact = |(label, value, code): (&'static str, String, bool)| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_color(t.text_muted).child(label))
                .child(
                    div()
                        .text_color(t.text)
                        .when(code, |value| value.font_family(mono.clone()))
                        .child(value),
                )
        };
        let reach = match &self.reach {
            Some(Loaded::Ready(reach)) => reach_sentence(role, reach),
            Some(Loaded::Failed(message)) => format!("Could not read the grants: {message}"),
            Some(Loaded::Loading) | None => "Reading the grants…".into(),
        };
        let name = role.name.clone();
        let edit = cx.listener({
            let name = name.clone();
            move |_, _, _, cx| cx.emit(UsersEvent::Edit(name.clone()))
        });
        let password = cx.listener({
            let name = name.clone();
            move |_, _, _, cx| cx.emit(UsersEvent::ResetPassword(name.clone()))
        });
        let grants = cx.listener({
            let name = name.clone();
            move |_, _, _, cx| cx.emit(UsersEvent::ShowGrants(name.clone()))
        });
        let (off, on) = (name.clone(), name.clone());
        let deactivate =
            cx.listener(move |_, _, _, cx| cx.emit(UsersEvent::SetLogin(off.clone(), false)));
        let reactivate =
            cx.listener(move |_, _, _, cx| cx.emit(UsersEvent::SetLogin(on.clone(), true)));
        panel
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .size(px(34.))
                            .flex_none()
                            .rounded_full()
                            .bg(ConnectionColor::Purple.fill())
                            .text_color(ConnectionColor::Purple.ink(t))
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family(mono.clone())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(layout::chrome(layout::TEXT_MD)))
                            .child(initials(&role.name)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(layout::chrome(14.)))
                                    .text_color(t.text)
                                    .child(role.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .text_color(t.text_muted)
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_x(px(14.))
                    .gap_y(px(10.))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .children(facts.into_iter().map(fact)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                            .text_color(t.text_muted)
                            .child("CAN REACH"),
                    )
                    .child(
                        div()
                            .debug_selector(|| "users-reach".into())
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .line_height(px(layout::chrome(18.)))
                            .text_color(t.text)
                            .child(reach),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(layout::SPACE_SM))
                    .child(outline_button("users-edit", "Edit…", t, t.text).on_click(edit))
                    .when(login, |actions| {
                        actions.child(
                            outline_button("users-password", "Reset Password…", t, t.text)
                                .on_click(password),
                        )
                    })
                    .child(
                        outline_button("users-grants", "Show Grants", t, t.text).on_click(grants),
                    )
                    .when(login && role.login, |actions| {
                        actions.child(
                            outline_button("users-deactivate", "Deactivate…", t, t.danger)
                                .border_color(t.danger.alpha(0.45))
                                .on_click(deactivate),
                        )
                    })
                    // A login turned off reads as one only where passwords can
                    // be read; elsewhere a role that cannot log in may be one,
                    // so letting it log in again is offered on every such row.
                    .when(!role.login, |actions| {
                        let label = match role.deactivated() {
                            true => "Reactivate…",
                            false => "Allow Login…",
                        };
                        actions.child(
                            outline_button("users-reactivate", label, t, t.text)
                                .on_click(reactivate),
                        )
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px(px(layout::SPACE_MD))
                    .py(px(10.))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.element_hover)
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .line_height(px(layout::chrome(17.)))
                    .text_color(t.text_muted)
                    .child(
                        "Every change is shown as the exact statement (CREATE ROLE, ALTER ROLE … \
                         NOLOGIN) and runs only after you approve it, at a connection mode that \
                         allows it.",
                    ),
            )
            .into_any_element()
    }

    fn reports_body(&self, t: Theme, mono: SharedString) -> AnyElement {
        let body = div()
            .id("users-reports")
            .flex_1()
            .overflow_y_scroll()
            .p(px(18.));
        let reports = match &self.reports {
            Some(Loaded::Ready(reports)) => reports,
            Some(Loaded::Failed(message)) => {
                return body
                    .text_color(t.danger)
                    .child(format!("Could not read the reports: {message}"))
                    .into_any_element();
            }
            _ => {
                return body
                    .text_color(t.text_muted)
                    .child("Reading the reports…")
                    .into_any_element();
            }
        };
        let cards = report_cards(reports).into_iter().map(|card| {
            let ink = match card.hue {
                Some(ConnectionColor::Red) => t.danger,
                Some(color) => color.ink(t),
                None => t.text,
            };
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .px(px(14.))
                .pt(px(10.))
                .pb(px(14.))
                .rounded(px(8.))
                .border_1()
                .border_color(t.border)
                .bg(t.panel_glass())
                .debug_selector({
                    let title = card.title;
                    move || format!("users-report-{title}")
                })
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(layout::SPACE_SM))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(px(layout::chrome(24.)))
                                .text_color(ink)
                                .child(card.count),
                        )
                        // Sat on the count's baseline, as the artboard draws it.
                        .child(
                            div()
                                .pb(px(layout::chrome(4.)))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(px(layout::chrome(layout::TEXT_MD)))
                                .text_color(t.text)
                                .child(card.title),
                        ),
                )
                .child(
                    div()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .line_height(px(layout::chrome(17.)))
                        .text_color(t.text_muted)
                        .child(card.detail),
                )
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_size(px(layout::chrome(11.5)))
                        .text_color(t.text)
                        .child(card.names),
                )
        });
        body.child(div().grid().grid_cols(3).gap(px(14.)).children(cards))
            .child(
                div()
                    .mt(px(layout::SPACE_LG))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_muted)
                    .child("Read from the server's own catalogs. Export any report as CSV."),
            )
            .into_any_element()
    }
}

/// One card of the Reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportCard {
    pub title: &'static str,
    pub count: String,
    pub detail: String,
    pub names: String,
    hue: Option<ConnectionColor>,
}

/// The Reports as cards, in the order the tab draws them. A report the
/// server cannot answer is left out rather than shown as empty.
pub fn report_cards(reports: &Reports) -> Vec<ReportCard> {
    let names = |names: &[String]| match names.is_empty() {
        true => "None".to_string(),
        false => names.join(", "),
    };
    let mut cards = vec![ReportCard {
        title: "Superusers",
        count: reports.superusers.len().to_string(),
        detail: "Roles that bypass every grant.".into(),
        names: names(&reports.superusers),
        hue: Some(ConnectionColor::Red),
    }];
    if let Some(idle) = &reports.idle {
        cards.push(ReportCard {
            title: "Idle for 90 days",
            count: idle.len().to_string(),
            detail: "Logins that can still connect but haven't.".into(),
            names: names(idle),
            hue: Some(ConnectionColor::Orange),
        });
    }
    cards.extend([
        ReportCard {
            title: "Passwords past expiry",
            count: reports.expired.len().to_string(),
            detail: "Their password is refused; any other way in still works.".into(),
            names: names(&reports.expired),
            hue: Some(ConnectionColor::Orange),
        },
        ReportCard {
            title: "Grants to PUBLIC",
            count: reports.public_grants.len().to_string(),
            detail: "Relations every role holds a privilege on.".into(),
            names: names(&reports.public_grants),
            hue: Some(ConnectionColor::Blue),
        },
        ReportCard {
            title: "Unused roles",
            count: reports.unused_roles.len().to_string(),
            detail: "Roles with no members that own and hold nothing here.".into(),
            names: names(&reports.unused_roles),
            hue: Some(ConnectionColor::Blue),
        },
    ]);
    let sessions: u64 = reports.sessions.iter().map(|(_, count)| count).sum();
    let by_login: Vec<String> = (reports.sessions.iter())
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    cards.push(ReportCard {
        title: "Sessions right now",
        count: sessions.to_string(),
        detail: match by_login.is_empty() {
            true => "No client is connected.".into(),
            false => format!("By login: {}.", by_login.join(", ")),
        },
        names: (reports.sessions.iter())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        hue: None,
    });
    cards
}

/// The Reports as CSV: a row per name each report lists.
pub fn reports_csv(reports: &Reports) -> String {
    let mut rows = vec![vec![
        "report".to_string(),
        "name".to_string(),
        "count".to_string(),
    ]];
    let mut push = |report: &str, names: &[String]| {
        for name in names {
            rows.push(vec![report.to_string(), name.clone(), String::new()]);
        }
    };
    push("Superusers", &reports.superusers);
    if let Some(idle) = &reports.idle {
        push("Idle for 90 days", idle);
    }
    push("Passwords past expiry", &reports.expired);
    push("Grants to PUBLIC", &reports.public_grants);
    push("Unused roles", &reports.unused_roles);
    for (name, count) in &reports.sessions {
        rows.push(vec![
            "Sessions right now".into(),
            name.clone(),
            count.to_string(),
        ]);
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|field| crate::export::csv_field(',', field))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn status_pill(status: Status, t: Theme) -> impl IntoElement {
    let (fill, ink) = match status {
        Status::Active => (ConnectionColor::Green.fill(), ConnectionColor::Green.ink(t)),
        Status::Deactivated => (ConnectionColor::Gray.fill(), ConnectionColor::Gray.ink(t)),
        Status::NeverLoggedIn => (
            ConnectionColor::Orange.fill(),
            ConnectionColor::Orange.ink(t),
        ),
        Status::Role => (
            ConnectionColor::Purple.fill(),
            ConnectionColor::Purple.ink(t),
        ),
    };
    div().flex().child(
        div()
            .px(px(layout::SPACE_SM))
            .py(px(2.))
            .rounded_full()
            .bg(fill)
            .text_color(ink)
            .text_size(px(layout::chrome(layout::TEXT_XS)))
            .font_weight(FontWeight::SEMIBOLD)
            .child(status.label()),
    )
}

/// A button drawn as a hairline outline, its label in `ink`.
fn outline_button(
    id: &'static str,
    label: &'static str,
    t: Theme,
    ink: crate::theme::color::Srgb,
) -> Button {
    ui::control(id, Tone::Quiet, Control::Compact)
        .px(px(11.))
        .border_1()
        .border_color(t.border_strong)
        .rounded(px(layout::RADIUS_CONTROL))
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(ink)
                .child(label),
        )
}

impl Render for UsersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let mono = fonts(cx).editor.clone();
        let body = match (&self.roles, self.reports_shown) {
            (Loaded::Ready(_), true) => self.reports_body(t, mono.clone()),
            (Loaded::Loading, _) => div()
                .p(px(18.))
                .text_color(t.text_muted)
                .child("Reading the roles…")
                .into_any_element(),
            (Loaded::Failed(message), _) => div()
                .p(px(18.))
                .text_color(t.danger)
                .child(format!("Could not read the roles: {message}"))
                .into_any_element(),
            (Loaded::Ready(roles), false) => {
                let roles = roles.clone();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.list(&roles, t, mono.clone(), cx))
                    .child(self.detail(&roles, t, mono.clone(), cx))
                    .into_any_element()
            }
        };
        // The planes are glass tints, the list and the Reports the content
        // plane's and the detail panel the editor's, as the other views'.
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(t.data_glass())
            .child(body);
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(t.text)
            .text_size(px(layout::chrome(layout::TEXT_MD)))
            .debug_selector(|| "users-view".into())
            .child(self.header(t, mono, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::roles::ReachEntry;

    fn role(name: &str, login: bool) -> Role {
        Role {
            name: name.into(),
            login,
            superuser: false,
            password_expires: None,
            expired: false,
            connection_limit: None,
            has_password: None,
            last_login: None,
        }
    }

    #[test]
    fn reach_reads_as_one_sentence_per_schema_and_route() {
        let reach = Reach {
            database: "analytics".into(),
            entries: vec![
                ReachEntry {
                    schema: "public".into(),
                    privilege: "SELECT".into(),
                    relations: 14,
                    through: vec!["reporting".into()],
                },
                ReachEntry {
                    schema: "public".into(),
                    privilege: "UPDATE".into(),
                    relations: 14,
                    through: vec!["reporting".into()],
                },
            ],
            owns: 0,
            predefined: Vec::new(),
        };
        assert_eq!(
            reach_sentence(&role("ana", true), &reach),
            "Select and Update on 14 relations in analytics.public, through reporting. \
             No other grant on a relation."
        );
        let reader = Reach {
            predefined: vec!["pg_read_all_data".into()],
            ..reach.clone()
        };
        assert!(
            reach_sentence(&role("ana", true), &reader)
                .ends_with("Holds pg_read_all_data. No other grant on a relation."),
        );
        let nothing = Reach {
            database: "analytics".into(),
            ..Reach::default()
        };
        assert_eq!(
            reach_sentence(&role("bo", true), &nothing),
            "No grant on a relation in analytics."
        );
    }

    #[test]
    fn a_turned_off_login_is_a_login_and_a_nologin_role_a_role() {
        let roles = Roles {
            roles: vec![
                role("reporting", false),
                Role {
                    has_password: Some(true),
                    ..role("intern", false)
                },
                role("ana", true),
            ],
            ..Roles::default()
        };
        let names: Vec<&str> = ordered(&roles)
            .iter()
            .map(|role| role.name.as_str())
            .collect();
        assert_eq!(names, ["ana", "intern", "reporting"]);
        assert_eq!(Status::of(&roles.roles[1], &roles), Status::Deactivated);
        assert_eq!(Status::of(&roles.roles[0], &roles), Status::Role);
        // Postgres records no logins, so no login is called never logged in.
        assert_eq!(Status::of(&roles.roles[2], &roles), Status::Active);
    }

    #[test]
    fn a_report_the_server_cannot_answer_is_left_out() {
        let reports = Reports {
            sessions: vec![("api".into(), 8), ("ana".into(), 2)],
            ..Reports::default()
        };
        let cards = report_cards(&reports);
        assert!(!cards.iter().any(|card| card.title == "Idle for 90 days"));
        let sessions = cards.last().unwrap();
        assert_eq!(sessions.count, "10");
        assert_eq!(sessions.detail, "By login: api 8, ana 2.");
        assert_eq!(
            reports_csv(&reports),
            "report,name,count\nSessions right now,api,8\nSessions right now,ana,2\n"
        );
        let odd = Reports {
            superusers: vec!["a\rb".into(), "c,d".into()],
            ..Reports::default()
        };
        assert_eq!(
            reports_csv(&odd),
            "report,name,count\nSuperusers,\"a\rb\",\nSuperusers,\"c,d\",\n"
        );
    }

    #[gpui::test]
    fn an_older_read_of_reach_or_reports_never_replaces_a_newer_one(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        let view = cx.new(|cx| UsersView::new("main".into(), cx));
        let reach = |owns| {
            Loaded::Ready(Reach {
                owns,
                ..Reach::default()
            })
        };
        view.update(cx, |view, cx| {
            view.selected = Some("ana".into());
            view.ask_reach("ana".into(), cx);
            let first = view.reach_request;
            // The same row asked again: the first answer is not this one's.
            view.ask_reach("ana".into(), cx);
            view.set_reach(first, reach(1), cx);
            assert!(matches!(view.reach, Some(Loaded::Loading)));
            view.set_reach(view.reach_request, reach(2), cx);
            assert!(matches!(&view.reach, Some(Loaded::Ready(read)) if read.owns == 2));
            view.set_reach(first, reach(1), cx);
            assert!(matches!(&view.reach, Some(Loaded::Ready(read)) if read.owns == 2));

            view.ask_reports(cx);
            let first = view.reports_request;
            view.ask_reports(cx);
            view.set_reports(first, Loaded::Failed("old".into()), cx);
            assert!(matches!(view.reports, Some(Loaded::Loading)));
            view.set_reports(view.reports_request, Loaded::Ready(Reports::default()), cx);
            assert!(view.reports().is_some());
        });
    }

    #[test]
    fn the_list_is_built_in_its_order_with_what_each_row_belongs_to() {
        let roles = Roles {
            roles: vec![
                role("reporting", false),
                role("bo", true),
                role("ana", true),
            ],
            memberships: vec![
                ("ana".into(), "reporting".into()),
                ("ana".into(), "billing".into()),
            ],
            ..Roles::default()
        };
        let listed = listing(&roles);
        let rows: Vec<(&str, &str)> = (listed.iter())
            .map(|row| (row.role.name.as_str(), row.member_of.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("ana", "billing, reporting"),
                ("bo", "—"),
                ("reporting", "—")
            ]
        );
    }

    #[test]
    fn the_keys_wrap_up_and_down_and_home_and_end_go_to_the_ends() {
        use Step::*;
        assert_eq!(stepped(0, None, Next), None);
        assert_eq!(stepped(3, Some(0), Next), Some(1));
        assert_eq!(stepped(3, Some(2), Next), Some(0), "Down wraps to the top");
        assert_eq!(
            stepped(3, Some(0), Previous),
            Some(2),
            "Up wraps to the end"
        );
        assert_eq!(stepped(3, Some(1), Previous), Some(0));
        assert_eq!(stepped(3, None, Next), Some(0));
        assert_eq!(stepped(3, None, Previous), Some(2));
        assert_eq!(stepped(3, Some(1), First), Some(0));
        assert_eq!(stepped(3, Some(1), Last), Some(2));
        assert_eq!(stepped(1, Some(0), Next), Some(0));
    }

    #[test]
    fn initials_take_a_letter_from_each_of_two_words() {
        assert_eq!(initials("analyst_ana"), "AA");
        assert_eq!(initials("postgres"), "PO");
    }
}
