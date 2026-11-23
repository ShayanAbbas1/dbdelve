//! What the Show Grants dialog draws: rows by privileges, each cell one of
//! four marks, built from what the server listed and nothing else. Pure, so
//! the reading of the catalog is tested apart from the window.

use std::collections::HashMap;

use crate::db::roles::{ObjectGrants, RoleGrants, Roles};
use crate::fuzzy::{Fuzzy, Query};

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

impl Mark {
    /// Which of two marks for one cell wins: a grant that can be passed on
    /// over one that cannot, over one held through a role, over none. So a
    /// privilege listed twice, by two grantors or two paths, reads the same
    /// whatever order the server listed them in.
    fn rank(self) -> u8 {
        match self {
            Mark::Grantable => 3,
            Mark::Granted => 2,
            Mark::Through => 1,
            Mark::None => 0,
        }
    }

    fn merge(self, other: Mark) -> Mark {
        match other.rank() > self.rank() {
            true => other,
            false => self,
        }
    }
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
    /// What tells it from every other row of the matrix, for its element
    /// ids: a role is listed again as a member of each role it belongs to,
    /// and two relations' `schema.name` can be spelled alike.
    pub(crate) key: String,
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

/// Who belongs to whom, read once per matrix rather than by a scan of every
/// membership at every step: each role's direct members, in name order.
struct Members<'a>(HashMap<&'a str, Vec<&'a str>>);

impl<'a> Members<'a> {
    fn of(roles: &'a Roles) -> Self {
        let mut by_role: HashMap<&str, Vec<&str>> = HashMap::new();
        for (member, role) in &roles.memberships {
            by_role
                .entry(role.as_str())
                .or_default()
                .push(member.as_str());
        }
        for members in by_role.values_mut() {
            members.sort_unstable();
            members.dedup();
        }
        Self(by_role)
    }

    fn of_role(&self, role: &str) -> &[&'a str] {
        self.0.get(role).map_or(&[], Vec::as_slice)
    }
}

/// The rows of an object's grants: superusers, the owner, every other role
/// holding something, then PUBLIC. `with_members` lists, under each role
/// with members, every role that belongs to it, at any depth. `filter`
/// keeps rows whose name it matches, through the one fuzzy matcher, with
/// the roles they are listed under.
pub(crate) fn object_rows(
    grants: &ObjectGrants,
    with_members: bool,
    filter: &str,
) -> Vec<MatrixRow> {
    let roles = &grants.roles;
    let members = Members::of(roles);
    let marks_of = |grantee: Option<&str>| -> Vec<Mark> {
        (grants.privileges.iter())
            .map(|privilege| {
                // One entry per grantor: grantable if any grantor gave it so.
                (grants.grants.iter())
                    .filter(|grant| {
                        grant.grantee.as_deref() == grantee && grant.privilege == *privilege
                    })
                    .map(|grant| match grant.grantable {
                        true => Mark::Grantable,
                        false => Mark::Granted,
                    })
                    .fold(Mark::None, Mark::merge)
            })
            .collect()
    };
    let everything = vec![Mark::Grantable; grants.privileges.len()];
    let mut top: Vec<MatrixRow> = Vec::new();
    for role in roles.roles.iter().filter(|role| role.superuser) {
        top.push(MatrixRow {
            key: role.name.clone(),
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
        let count = members.of_role(name).len();
        let via = if name == grants.owner {
            "owner".to_string()
        } else {
            match (who(roles, name), count) {
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
            key: name.to_string(),
            name: name.to_string(),
            via,
            who: who(roles, name),
            member: false,
            opened: false,
            marks,
        });
    }
    top.push(MatrixRow {
        // No role's name: PUBLIC is a keyword, never one.
        key: "\u{1f}PUBLIC".into(),
        name: "PUBLIC".into(),
        via: "every role".into(),
        who: Who::Public,
        member: false,
        opened: false,
        marks: marks_of(None),
    });

    let query = Query::new(filter);
    let mut fuzzy = Fuzzy::new();
    let mut matches = |name: &str| fuzzy.matches(&query, name);
    let mut rows = Vec::new();
    for mut row in top {
        let listed = match (with_members, row.who) {
            (true, Who::Login | Who::NoLogin) => members_of(roles, &members, &row.name, &row.marks),
            _ => Vec::new(),
        };
        let kept = matches(&row.name);
        let shown: Vec<MatrixRow> = match kept {
            true => listed,
            false => listed
                .into_iter()
                .filter(|member| matches(&member.name))
                .collect(),
        };
        if !kept && shown.is_empty() {
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
/// role holds only where every membership on its way inherits; one reached
/// by several paths holds it if any of them does, and is said to come
/// through that one.
fn members_of(roles: &Roles, members: &Members, role: &str, marks: &[Mark]) -> Vec<MatrixRow> {
    let through_marks = |inherits: bool| -> Vec<Mark> {
        (marks.iter())
            .map(|mark| match (mark, inherits) {
                (Mark::None, _) | (_, false) => Mark::None,
                _ => Mark::Through,
            })
            .collect()
    };
    let via = |name: &str, through: Option<&str>| match through {
        None => format!("member of {role} · {}", kind_text(roles, name)),
        Some(through) => format!(
            "member of {role} · {}, through {through}",
            kind_text(roles, name)
        ),
    };
    let mut rows: Vec<MatrixRow> = Vec::new();
    // Each row's place and whether it inherits, by name.
    let mut placed: HashMap<String, (usize, bool)> = HashMap::new();
    let mut stack: Vec<(&str, Option<&str>, bool)> = (members.of_role(role).iter().rev())
        .map(|member| (*member, None, roles.inherits(member, role)))
        .collect();
    while let Some((name, through, inherits)) = stack.pop() {
        if name == role {
            continue;
        }
        match placed.get_mut(name) {
            // Reached again with nothing new: an inheriting row stays one,
            // and a path that does not inherit adds nothing.
            Some((_, already)) if *already || !inherits => continue,
            // Raised: this path inherits where the first did not. Once per
            // row, so the walk still ends; its members are walked again to
            // raise them too.
            Some((at, already)) => {
                *already = true;
                rows[*at].marks = through_marks(true);
                rows[*at].via = via(name, through);
            }
            None => {
                placed.insert(name.to_string(), (rows.len(), inherits));
                rows.push(MatrixRow {
                    key: format!("{role}\u{1f}{name}"),
                    name: name.to_string(),
                    via: via(name, through),
                    who: who(roles, name),
                    member: true,
                    opened: false,
                    marks: through_marks(inherits),
                });
            }
        }
        for member in members.of_role(name).iter().rev() {
            stack.push((member, Some(name), inherits && roles.inherits(member, name)));
        }
    }
    rows
}

/// What Show Grants of a role says when it lists no relation: why it may
/// still reach them, as a superuser, an owner or through a predefined role,
/// before saying it holds nothing.
pub(crate) fn role_grants_empty(grants: &RoleGrants) -> String {
    let beyond = &grants.beyond;
    if beyond.superuser {
        return "Superuser: it bypasses every grant, so it reaches every relation.".into();
    }
    let mut said = Vec::new();
    if beyond.owns > 0 {
        said.push(format!(
            "Owns {}, and holds everything on them.",
            plural(beyond.owns as usize, "relation")
        ));
    }
    if !beyond.predefined.is_empty() {
        said.push(format!(
            "Holds {}, which reaches further than any grant.",
            beyond.predefined.join(", ")
        ));
    }
    said.push(format!(
        "It holds no grant on {} relation of {}.",
        match said.is_empty() {
            true => "any",
            false => "any other",
        },
        grants.database
    ));
    said.join(" ")
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

/// A role's own grants, a row per relation it holds anything on, told
/// apart by schema and name and not by their spelling joined: `"a.b".c` and
/// `a."b.c"` are two rows. Each cell takes the strongest of its marks
/// ([`Mark::merge`]), so a grant of its own wins over the same one held
/// through a role, and one a grantor made grantable over one another did
/// not.
pub(crate) fn role_rows(grants: &RoleGrants, columns: &[String], filter: &str) -> Vec<MatrixRow> {
    let query = Query::new(filter);
    let mut fuzzy = Fuzzy::new();
    let mut rows: Vec<MatrixRow> = Vec::new();
    let mut through_of: Vec<Vec<String>> = Vec::new();
    let mut placed: HashMap<(&str, &str), usize> = HashMap::new();
    for held in &grants.held {
        let key = (held.schema.as_str(), held.relation.as_str());
        let at = match placed.get(&key) {
            Some(at) => *at,
            None => {
                let name = format!("{}.{}", held.schema, held.relation);
                if !fuzzy.matches(&query, &name) {
                    continue;
                }
                placed.insert(key, rows.len());
                rows.push(MatrixRow {
                    key: format!("{}\u{1f}{}", held.schema, held.relation),
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
        *cell = cell.merge(mark);
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
        let rows = object_rows(&orders(), true, "ANA");
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["reporting", "ana", "analytics_team"]);
    }

    #[test]
    fn a_member_reached_again_through_an_inheriting_path_holds_it_and_so_do_its_members() {
        // R holds SELECT. A belongs to R without inheriting, B with. X
        // belongs to both and is reached through A first; Y belongs to X.
        let grants = ObjectGrants {
            owner: "owner".into(),
            privileges: vec!["SELECT".into()],
            grants: vec![grant(Some("r"), "SELECT", false)],
            roles: Roles {
                roles: ["owner", "r", "a", "b", "x", "y"]
                    .into_iter()
                    .map(|name| role(name, false, false))
                    .collect(),
                memberships: vec![
                    ("a".into(), "r".into()),
                    ("b".into(), "r".into()),
                    ("x".into(), "a".into()),
                    ("x".into(), "b".into()),
                    ("y".into(), "x".into()),
                ],
                not_inherited: vec![("a".into(), "r".into())],
                records_logins: false,
            },
        };
        let rows = object_rows(&grants, true, "");
        let row = |name: &str| {
            (rows.iter())
                .find(|row| row.member && row.name == name)
                .unwrap_or_else(|| panic!("{name} is listed"))
        };
        assert_eq!(row("a").marks, [Mark::None]);
        assert_eq!(row("b").marks, [Mark::Through]);
        assert_eq!(row("x").marks, [Mark::Through]);
        assert_eq!(row("x").via, "member of r · role, through b");
        assert_eq!(row("y").marks, [Mark::Through], "raised with x");
        assert_eq!(rows.iter().filter(|row| row.name == "x").count(), 1);
    }

    #[test]
    fn every_row_of_the_matrix_has_a_key_of_its_own() {
        let mut grants = orders();
        // ana holds a grant of its own and is listed under reporting too.
        grants.grants.push(grant(Some("ana"), "INSERT", false));
        let rows = object_rows(&grants, true, "");
        assert!(rows.iter().filter(|row| row.name == "ana").count() > 1);
        let mut keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "{keys:?}");
    }

    #[test]
    fn two_relations_spelled_alike_once_joined_are_two_rows() {
        let held = |schema: &str, relation: &str| HeldGrant {
            schema: schema.into(),
            relation: relation.into(),
            privilege: "SELECT".into(),
            grantable: false,
            through: None,
        };
        let grants = RoleGrants {
            database: "d".into(),
            beyond: Default::default(),
            held: vec![held("a.b", "c"), held("a", "b.c")],
        };
        let rows = role_rows(&grants, &role_columns(&grants), "");
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].key, rows[1].key);
    }

    #[test]
    fn a_privilege_from_two_grantors_shows_the_stronger_mark_in_either_order() {
        let held = |grantable: bool| HeldGrant {
            schema: "public".into(),
            relation: "orders".into(),
            privilege: "SELECT".into(),
            grantable,
            through: None,
        };
        for order in [[false, true], [true, false]] {
            let grants = RoleGrants {
                database: "d".into(),
                beyond: Default::default(),
                held: order.into_iter().map(held).collect(),
            };
            let rows = role_rows(&grants, &role_columns(&grants), "");
            assert_eq!(rows[0].marks, [Mark::Grantable], "{order:?}");
        }
        let mut object = orders();
        object
            .grants
            .insert(0, grant(Some("reporting"), "SELECT", true));
        let rows = object_rows(&object, false, "");
        let reporting = rows.iter().find(|row| row.name == "reporting").unwrap();
        assert_eq!(reporting.marks[0], Mark::Grantable);
    }

    #[test]
    fn the_filters_match_through_the_fuzzy_matcher() {
        let names = |rows: Vec<MatrixRow>| -> Vec<String> {
            rows.into_iter().map(|row| row.name).collect()
        };
        assert_eq!(names(object_rows(&orders(), false, "rptg")), ["reporting"]);
        let grants = RoleGrants {
            database: "d".into(),
            beyond: Default::default(),
            held: vec![HeldGrant {
                schema: "sales".into(),
                relation: "orders".into(),
                privilege: "SELECT".into(),
                grantable: false,
                through: None,
            }],
        };
        assert_eq!(
            names(role_rows(&grants, &role_columns(&grants), "slsord")),
            ["sales.orders"]
        );
    }

    #[test]
    fn an_empty_list_of_a_roles_grants_says_what_reaches_past_them() {
        use crate::db::roles::Beyond;
        let grants = |beyond: Beyond| RoleGrants {
            database: "shop".into(),
            held: Vec::new(),
            beyond,
        };
        assert_eq!(
            role_grants_empty(&grants(Beyond::default())),
            "It holds no grant on any relation of shop."
        );
        assert!(
            role_grants_empty(&grants(Beyond {
                superuser: true,
                ..Beyond::default()
            }))
            .starts_with("Superuser")
        );
        assert_eq!(
            role_grants_empty(&grants(Beyond {
                owns: 3,
                predefined: vec!["pg_read_all_data".into()],
                ..Beyond::default()
            })),
            "Owns 3 relations, and holds everything on them. Holds pg_read_all_data, which \
             reaches further than any grant. It holds no grant on any other relation of shop."
        );
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
            beyond: Default::default(),
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
