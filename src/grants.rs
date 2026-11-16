//! What the Show Grants dialog draws: rows by privileges, each cell one of
//! four marks, built from what the server listed and nothing else. Pure, so
//! the reading of the catalog is tested apart from the window.

use crate::db::roles::{ObjectGrants, RoleGrants, Roles};

/// One cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Granted,
    /// Granted, and the holder may grant it on.
    Grantable,
    /// Held through a role the row's role belongs to.
    Through,
    None,
}

/// What a row's icon says of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Who {
    Login,
    NoLogin,
    Superuser,
    Public,
    /// A relation, in a role's own grants.
    Object,
}

impl Who {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Who::Login => "Can log in",
            Who::NoLogin => "Cannot log in",
            Who::Superuser => "Superuser",
            Who::Public => "Every role",
            Who::Object => "Relation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MatrixRow {
    pub(crate) name: String,
    /// The line under the name: what the row is and how it holds them.
    pub(crate) via: String,
    pub(crate) who: Who,
    /// A member listed under the role it belongs to.
    pub(crate) member: bool,
    /// Whether members are listed under it, for the chevron.
    pub(crate) opened: bool,
    pub(crate) marks: Vec<Mark>,
}

/// What a column header says of a privilege the server spells `SELECT`.
pub(crate) fn column_label(privilege: &str) -> String {
    let mut label = privilege.to_lowercase();
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    label
}

fn who(roles: &Roles, name: &str) -> Who {
    match roles.role(name) {
        Some(role) if role.superuser => Who::Superuser,
        Some(role) if role.login || role.deactivated() => Who::Login,
        _ => Who::NoLogin,
    }
}

fn kind_text(roles: &Roles, name: &str) -> &'static str {
    match who(roles, name) {
        Who::NoLogin => "role",
        _ => "login",
    }
}

fn plural(count: usize, one: &str) -> String {
    match count {
        1 => format!("1 {one}"),
        n => format!("{n} {one}s"),
    }
}

/// The rows of an object's grants: superusers, the owner, every other role
/// holding something, then PUBLIC. `with_members` lists, under each role
/// with members, every role that belongs to it, at any depth. `filter`
/// keeps rows whose name holds it, ignoring case, with the roles they are
/// listed under.
pub(crate) fn object_rows(
    grants: &ObjectGrants,
    with_members: bool,
    filter: &str,
) -> Vec<MatrixRow> {
    let roles = &grants.roles;
    let marks_of = |grantee: Option<&str>| -> Vec<Mark> {
        (grants.privileges.iter())
            .map(|privilege| {
                // One entry per grantor: grantable if any grantor gave it so.
                let held: Vec<_> = (grants.grants.iter())
                    .filter(|grant| {
                        grant.grantee.as_deref() == grantee && grant.privilege == *privilege
                    })
                    .collect();
                match held.as_slice() {
                    [] => Mark::None,
                    held if held.iter().any(|grant| grant.grantable) => Mark::Grantable,
                    _ => Mark::Granted,
                }
            })
            .collect()
    };
    let everything = vec![Mark::Grantable; grants.privileges.len()];
    let mut top: Vec<MatrixRow> = Vec::new();
    for role in roles.roles.iter().filter(|role| role.superuser) {
        top.push(MatrixRow {
            name: role.name.clone(),
            via: "superuser · bypasses grants".into(),
            who: Who::Superuser,
            member: false,
            opened: false,
            marks: everything.clone(),
        });
    }
    let superuser = |name: &str| roles.role(name).is_some_and(|role| role.superuser);
    let mut grantees: Vec<&str> = (grants.grants.iter())
        .filter_map(|grant| grant.grantee.as_deref())
        .filter(|name| *name != grants.owner && !superuser(name))
        .collect();
    grantees.sort_unstable();
    grantees.dedup();
    let owner = (!superuser(&grants.owner)).then_some(grants.owner.as_str());
    for name in owner.into_iter().chain(grantees) {
        let members = roles.members(name);
        let via = if name == grants.owner {
            "owner".to_string()
        } else {
            match (who(roles, name), members.len()) {
                (Who::NoLogin, 0) => "role, cannot log in".into(),
                (Who::NoLogin, n) => format!("role, cannot log in · {}", plural(n, "member")),
                _ => "login · direct".into(),
            }
        };
        let mut marks = marks_of(Some(name));
        // An owner may grant whatever it holds on its own object, which its
        // ACL does not spell out with a grant option.
        if name == grants.owner {
            for mark in &mut marks {
                if *mark == Mark::Granted {
                    *mark = Mark::Grantable;
                }
            }
        }
        top.push(MatrixRow {
            name: name.to_string(),
            via,
            who: who(roles, name),
            member: false,
            opened: false,
            marks,
        });
    }
    top.push(MatrixRow {
        name: "PUBLIC".into(),
        via: "every role".into(),
        who: Who::Public,
        member: false,
        opened: false,
        marks: marks_of(None),
    });

    let needle = filter.trim().to_lowercase();
    let matches = |name: &str| needle.is_empty() || name.to_lowercase().contains(&needle);
    let mut rows = Vec::new();
    for mut row in top {
        let listed = match (with_members, row.who) {
            (true, Who::Login | Who::NoLogin) => members_of(roles, &row.name, &row.marks),
            _ => Vec::new(),
        };
        let shown: Vec<MatrixRow> = match matches(&row.name) {
            true => listed,
            false => listed
                .into_iter()
                .filter(|member| matches(&member.name))
                .collect(),
        };
        if !matches(&row.name) && shown.is_empty() {
            continue;
        }
        row.opened = !shown.is_empty();
        rows.push(row);
        rows.extend(shown);
    }
    rows
}

/// Every role that belongs to `role`, depth first, each named by the role
/// it is listed under and the one it came through. A member holds what the
/// role holds only where every membership on its way inherits.
fn members_of(roles: &Roles, role: &str, marks: &[Mark]) -> Vec<MatrixRow> {
    let mut rows = Vec::new();
    let mut stack: Vec<(String, Option<String>, bool)> = roles
        .members(role)
        .into_iter()
        .rev()
        .map(|member| (member.to_string(), None, roles.inherits(member, role)))
        .collect();
    let through_marks = |inherits: bool| -> Vec<Mark> {
        (marks.iter())
            .map(|mark| match (mark, inherits) {
                (Mark::None, _) | (_, false) => Mark::None,
                _ => Mark::Through,
            })
            .collect()
    };
    let mut seen = vec![role.to_string()];
    while let Some((name, through, inherits)) = stack.pop() {
        if seen.contains(&name) {
            // Reached again: a path that inherits wins over one that does not.
            if inherits
                && let Some(row) = rows
                    .iter_mut()
                    .find(|row: &&mut MatrixRow| row.name == name)
            {
                row.marks = through_marks(true);
            }
            continue;
        }
        seen.push(name.clone());
        let via = match &through {
            None => format!("member of {role} · {}", kind_text(roles, &name)),
            Some(through) => format!(
                "member of {role} · {}, through {through}",
                kind_text(roles, &name)
            ),
        };
        rows.push(MatrixRow {
            name: name.clone(),
            via,
            who: who(roles, &name),
            member: true,
            opened: false,
            marks: through_marks(inherits),
        });
        for member in roles.members(&name).into_iter().rev() {
            stack.push((
                member.to_string(),
                Some(name.clone()),
                inherits && roles.inherits(member, &name),
            ));
        }
    }
    rows
}

/// The columns of a role's own grants: the privileges it holds anywhere, in
/// the order a table's are listed, then any other.
pub(crate) fn role_columns(grants: &RoleGrants) -> Vec<String> {
    const ORDER: [&str; 8] = [
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "TRUNCATE",
        "REFERENCES",
        "TRIGGER",
        "USAGE",
    ];
    let mut columns: Vec<String> = ORDER
        .iter()
        .filter(|privilege| grants.held.iter().any(|held| held.privilege == **privilege))
        .map(|privilege| privilege.to_string())
        .collect();
    for held in &grants.held {
        if !columns.contains(&held.privilege) {
            columns.push(held.privilege.clone());
        }
    }
    columns
}

/// A role's own grants, a row per relation it holds anything on. A grant
/// of its own wins over the same one held through a role.
pub(crate) fn role_rows(grants: &RoleGrants, columns: &[String], filter: &str) -> Vec<MatrixRow> {
    let needle = filter.trim().to_lowercase();
    let mut rows: Vec<MatrixRow> = Vec::new();
    let mut through_of: Vec<Vec<String>> = Vec::new();
    for held in &grants.held {
        let name = format!("{}.{}", held.schema, held.relation);
        if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
            continue;
        }
        let at = match rows.iter().position(|row| row.name == name) {
            Some(at) => at,
            None => {
                rows.push(MatrixRow {
                    name,
                    via: String::new(),
                    who: Who::Object,
                    member: false,
                    opened: false,
                    marks: vec![Mark::None; columns.len()],
                });
                through_of.push(Vec::new());
                rows.len() - 1
            }
        };
        let Some(column) = columns.iter().position(|column| *column == held.privilege) else {
            continue;
        };
        let mark = match (&held.through, held.grantable) {
            (Some(role), _) => {
                if !through_of[at].contains(role) {
                    through_of[at].push(role.clone());
                }
                Mark::Through
            }
            (None, true) => Mark::Grantable,
            (None, false) => Mark::Granted,
        };
        let cell = &mut rows[at].marks[column];
        if *cell == Mark::None || *cell == Mark::Through {
            *cell = mark;
        }
    }
    for (row, through) in rows.iter_mut().zip(through_of) {
        let direct = row
            .marks
            .iter()
            .any(|mark| matches!(mark, Mark::Granted | Mark::Grantable));
        row.via = match (direct, through.is_empty()) {
            (true, true) => "direct".into(),
            (false, false) => format!("through {}", through.join(", ")),
            (true, false) => format!("direct, and through {}", through.join(", ")),
            (false, true) => String::new(),
        };
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::roles::{Grant, HeldGrant, Role};

    fn role(name: &str, login: bool, superuser: bool) -> Role {
        Role {
            name: name.into(),
            login,
            superuser,
            password_expires: None,
            expired: false,
            connection_limit: None,
            has_password: None,
            last_login: None,
        }
    }

    fn grant(grantee: Option<&str>, privilege: &str, grantable: bool) -> Grant {
        Grant {
            grantee: grantee.map(str::to_string),
            privilege: privilege.into(),
            grantable,
        }
    }

    fn orders() -> ObjectGrants {
        ObjectGrants {
            owner: "app_owner".into(),
            privileges: vec!["SELECT".into(), "INSERT".into()],
            grants: vec![
                grant(Some("app_owner"), "SELECT", true),
                grant(Some("app_owner"), "INSERT", true),
                grant(Some("reporting"), "SELECT", false),
            ],
            roles: Roles {
                roles: vec![
                    role("postgres", true, true),
                    role("app_owner", true, false),
                    role("reporting", false, false),
                    role("analytics_team", false, false),
                    role("ana", true, false),
                    role("bi", false, false),
                ],
                memberships: vec![
                    ("ana".into(), "reporting".into()),
                    ("analytics_team".into(), "reporting".into()),
                    ("bi".into(), "analytics_team".into()),
                ],
                not_inherited: vec![("bi".into(), "analytics_team".into())],
                records_logins: false,
            },
        }
    }

    #[test]
    fn rows_run_superusers_owner_grantees_then_public() {
        let rows = object_rows(&orders(), false, "");
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["postgres", "app_owner", "reporting", "PUBLIC"]);
        assert_eq!(rows[0].marks, [Mark::Grantable, Mark::Grantable]);
        assert_eq!(rows[2].via, "role, cannot log in · 2 members");
        assert_eq!(rows[2].marks, [Mark::Granted, Mark::None]);
        assert_eq!(rows[3].marks, [Mark::None, Mark::None]);
    }

    #[test]
    fn members_hold_through_their_role_only_where_every_step_inherits() {
        let rows = object_rows(&orders(), true, "");
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "postgres",
                "app_owner",
                "reporting",
                "ana",
                "analytics_team",
                "bi",
                "PUBLIC"
            ]
        );
        assert!(rows[2].opened);
        assert_eq!(rows[3].via, "member of reporting · login");
        assert_eq!(rows[3].marks, [Mark::Through, Mark::None]);
        assert_eq!(
            rows[5].via,
            "member of reporting · role, through analytics_team"
        );
        assert_eq!(rows[5].marks, [Mark::None, Mark::None]);
    }

    #[test]
    fn a_filter_keeps_a_member_with_the_role_it_is_listed_under() {
        let rows = object_rows(&orders(), true, "AN");
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["reporting", "ana", "analytics_team"]);
    }

    #[test]
    fn a_roles_own_grant_wins_over_one_held_through_a_role() {
        let held = |relation: &str, privilege: &str, through: Option<&str>| HeldGrant {
            schema: "public".into(),
            relation: relation.into(),
            privilege: privilege.into(),
            grantable: false,
            through: through.map(str::to_string),
        };
        let grants = RoleGrants {
            database: "d".into(),
            held: vec![
                held("orders", "SELECT", None),
                held("orders", "SELECT", Some("reporting")),
                held("orders", "UPDATE", Some("app_rw")),
            ],
        };
        let columns = role_columns(&grants);
        assert_eq!(columns, ["SELECT", "UPDATE"]);
        let rows = role_rows(&grants, &columns, "");
        assert_eq!(rows[0].marks, [Mark::Granted, Mark::Through]);
        assert_eq!(rows[0].via, "direct, and through reporting, app_rw");
    }
}
