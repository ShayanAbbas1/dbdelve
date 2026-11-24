//! What the Show Grants dialog draws: rows by privileges, each cell one of
//! four marks, built from what the server listed and nothing else. Pure, so
//! the reading of the catalog is tested apart from the window.

use std::collections::{HashMap, HashSet};

use crate::db::roles::{
    DatabaseAccess, HeldGrant, ObjectGrants, Right, RoleAccess, RoleGrants, Roles, SchemaAccess,
    Source,
};
use crate::fuzzy::{Fuzzy, Query};

/// One cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Granted,
    /// Granted, and the holder may grant it on.
    Grantable,
    /// Held through a role the row's role belongs to.
    Through,
    /// Held because every role holds it.
    Public,
    /// Granted on a relation in a schema the role has no `USAGE` on, so it
    /// reaches nothing.
    Blocked,
    None,
}

impl Mark {
    /// Which of two marks for one cell wins: a grant that can be passed on
    /// over one that cannot, over one held through a role, over none. So a
    /// privilege listed twice, by two grantors or two paths, reads the same
    /// whatever order the server listed them in.
    fn rank(self) -> u8 {
        match self {
            Mark::Grantable => 5,
            Mark::Granted => 4,
            Mark::Through => 3,
            Mark::Public => 2,
            Mark::Blocked => 1,
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
    /// A database a role's access is listed for.
    Database,
    Schema,
    /// A line of text under a database: what its rows leave out.
    Note,
}

impl Who {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Who::Login => "Can log in",
            Who::NoLogin => "Cannot log in",
            Who::Superuser => "Superuser",
            Who::Public => "Every role",
            Who::Object => "Relation",
            Who::Database => "Database",
            Who::Schema => "Schema",
            Who::Note => "Note",
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
    /// How far in it is drawn: a schema under its database, a relation under
    /// its schema.
    pub(crate) depth: u8,
    /// A member listed under the role it belongs to.
    pub(crate) member: bool,
    /// Whether members are listed under it, for the chevron.
    pub(crate) opened: bool,
    pub(crate) marks: Vec<Mark>,
}

/// Whether `row` is drawn under `parent`, and goes with it when it is
/// collapsed: deeper than it (a schema under its database, a relation under
/// its schema), or a member listed under a role that is not one itself.
fn under(parent: &MatrixRow, row: &MatrixRow) -> bool {
    row.depth > parent.depth || (row.member && !parent.member)
}

/// `rows` with what is under every collapsed row left out. Only a row with
/// rows under it (`opened`) folds, and a fold inside a folded one is hidden
/// with it.
pub(crate) fn folded(rows: &[MatrixRow], collapsed: &HashSet<String>) -> Vec<MatrixRow> {
    let mut shown = Vec::with_capacity(rows.len());
    let mut fold: Option<&MatrixRow> = None;
    for row in rows {
        if fold.is_some_and(|parent| under(parent, row)) {
            continue;
        }
        fold = (row.opened && collapsed.contains(&row.key)).then_some(row);
        shown.push(row.clone());
    }
    shown
}

/// What a column header says of a privilege the server spells `SELECT`.
pub(crate) fn column_label(privilege: &str) -> String {
    // The column is as wide as the longest of the others.
    let privilege = if privilege == "TEMPORARY" {
        "TEMP"
    } else {
        privilege
    };
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
            depth: 0,
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
            depth: 0,
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
        depth: 0,
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
                    depth: 0,
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

/// How many databases Show Grants of a role reads at most, one session each
/// ([`crate::workspace::Workspace::run_routed`] keeps them open). The rest are
/// listed and said not to be read.
///
/// ponytail: a flat cap. The upgrade is reading a database when its section
/// is opened, once the dialog has collapsible sections.
pub(crate) const MAX_DATABASES_READ: usize = 20;

/// Why a database of a role's access was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Skipped {
    /// It cannot connect: nothing inside is reached, so nothing is read.
    NoConnect,
    /// It connects, but the profile's database filter hides it: a hidden
    /// database is never read.
    Hidden,
    /// A superuser holds everything in it, which a list of grants would
    /// only understate.
    Superuser,
    /// Past [`MAX_DATABASES_READ`].
    Limit,
}

/// Where the read of one database stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Read {
    /// Still to be read, or being read.
    Pending,
    Done(RoleGrants),
    /// The read failed; the others are not affected.
    Failed(String),
    Skipped(Skipped),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DatabaseSection {
    pub(crate) access: DatabaseAccess,
    pub(crate) read: Read,
}

/// Show Grants of a role: every database of the server with what the role
/// holds on it, and what has been read inside each so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoleView {
    pub(crate) sections: Vec<DatabaseSection>,
}

impl RoleView {
    /// Decide which databases are read: those it can connect to that the
    /// database filter (`shows`) lets through, up to [`MAX_DATABASES_READ`].
    pub(crate) fn plan(access: RoleAccess, shows: impl Fn(&str) -> bool) -> Self {
        let mut pending = 0;
        let sections = access
            .databases
            .into_iter()
            .map(|database| {
                let read = if !database.connects() {
                    Read::Skipped(Skipped::NoConnect)
                } else if !shows(&database.name) {
                    Read::Skipped(Skipped::Hidden)
                } else if access.superuser {
                    Read::Skipped(Skipped::Superuser)
                } else if pending >= MAX_DATABASES_READ {
                    Read::Skipped(Skipped::Limit)
                } else {
                    pending += 1;
                    Read::Pending
                };
                DatabaseSection {
                    access: database,
                    read,
                }
            })
            .collect();
        Self { sections }
    }

    /// The databases still to read, in the order they are listed.
    pub(crate) fn pending(&self) -> Vec<String> {
        (self.sections.iter())
            .filter(|section| section.read == Read::Pending)
            .map(|section| section.access.name.clone())
            .collect()
    }

    /// What reading `database` answered. Nothing else changes.
    pub(crate) fn land(&mut self, database: &str, result: Result<RoleGrants, String>) {
        if let Some(section) = (self.sections.iter_mut())
            .find(|section| section.access.name == database && section.read == Read::Pending)
        {
            section.read = match result {
                Ok(grants) => Read::Done(grants),
                Err(message) => Read::Failed(message),
            };
        }
    }
}

/// The order a table's privileges are listed in, those of a database and a
/// schema before them.
const PRIVILEGE_ORDER: [&str; 11] = [
    "CONNECT",
    "TEMPORARY",
    "USAGE",
    "CREATE",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];

/// The columns of a role's access: the privileges it holds anywhere in the
/// view, in [`PRIVILEGE_ORDER`], then any other.
pub(crate) fn role_columns(view: &RoleView) -> Vec<String> {
    let mut held: Vec<&str> = Vec::new();
    for section in &view.sections {
        held.extend(section.access.rights.iter().map(|r| r.privilege.as_str()));
        if let Read::Done(grants) = &section.read {
            for schema in &grants.schemas {
                held.extend(schema.rights.iter().map(|r| r.privilege.as_str()));
            }
            held.extend(grants.held.iter().map(|grant| grant.privilege.as_str()));
        }
    }
    let mut columns: Vec<String> = PRIVILEGE_ORDER
        .iter()
        .filter(|privilege| held.contains(privilege))
        .map(|privilege| privilege.to_string())
        .collect();
    for privilege in held {
        if !columns.iter().any(|column| column == privilege) {
            columns.push(privilege.to_string());
        }
    }
    columns
}

/// What one way of holding a privilege shows as: the strongest source's.
fn source_mark(sources: &[Source]) -> Mark {
    match sources.first() {
        Some(Source::Superuser | Source::Owner(_) | Source::Direct) => Mark::Granted,
        Some(Source::Via(_) | Source::Predefined(_) | Source::Implicit) => Mark::Through,
        Some(Source::Public) => Mark::Public,
        None => Mark::None,
    }
}

fn rights_marks(rights: &[Right], columns: &[String]) -> Vec<Mark> {
    (columns.iter())
        .map(|column| {
            (rights.iter())
                .find(|right| right.privilege == *column)
                .map_or(Mark::None, |right| source_mark(&right.sources))
        })
        .collect()
}

/// `connect PUBLIC · create owner`: each right, with where it comes from.
fn rights_text(rights: &[Right]) -> String {
    (rights.iter())
        .map(|right| {
            let sources: Vec<String> = right.sources.iter().map(Source::label).collect();
            format!("{} {}", right.privilege.to_lowercase(), sources.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A database's row: what the role holds on the database itself, and
/// whether and why what is inside it is listed.
fn database_row(section: &DatabaseSection, columns: &[String]) -> MatrixRow {
    let access = &section.access;
    let rights = rights_text(&access.rights);
    let status = match &section.read {
        Read::Pending => Some("reading…".to_string()),
        Read::Failed(message) => Some(format!("could not read: {message}")),
        Read::Done(_) => None,
        Read::Skipped(Skipped::NoConnect) if !access.allows_connections => {
            Some("takes no connections: not read".into())
        }
        Read::Skipped(Skipped::NoConnect) => Some("no CONNECT: not read".into()),
        Read::Skipped(Skipped::Hidden) => Some("hidden by the filter: not read".into()),
        Read::Skipped(Skipped::Superuser) => Some("superuser: holds everything, not read".into()),
        Read::Skipped(Skipped::Limit) => Some(format!(
            "not read: only the first {MAX_DATABASES_READ} databases are, narrow Filter Databases"
        )),
    };
    let blocked = matches!(section.read, Read::Skipped(Skipped::NoConnect));
    MatrixRow {
        key: format!("{}\u{1f}", access.name),
        name: access.name.clone(),
        via: [status, (!blocked && !rights.is_empty()).then_some(rights)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        who: Who::Database,
        depth: 0,
        member: false,
        opened: matches!(section.read, Read::Done(_)),
        marks: match blocked {
            true => vec![Mark::None; columns.len()],
            false => rights_marks(&access.rights, columns),
        },
    }
}

fn schema_row(database: &str, schema: &SchemaAccess, columns: &[String]) -> MatrixRow {
    let owns =
        (schema.owns > 0).then(|| format!("owns {}", plural(schema.owns as usize, "relation")));
    let rights = rights_text(&schema.rights);
    MatrixRow {
        key: format!("{database}\u{1f}{}\u{1f}", schema.name),
        name: schema.name.clone(),
        via: [
            Some(rights).filter(|rights| !rights.is_empty()),
            owns,
            (!schema.usable()).then(|| "no USAGE".to_string()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
        who: Who::Schema,
        depth: 1,
        member: false,
        opened: false,
        marks: rights_marks(&schema.rights, columns),
    }
}

fn note(database: &str, index: usize, text: String) -> MatrixRow {
    MatrixRow {
        key: format!("{database}\u{1f}\u{1f}note{index}"),
        name: text,
        via: String::new(),
        who: Who::Note,
        depth: 1,
        member: false,
        opened: false,
        marks: Vec::new(),
    }
}

/// What a database's rows leave out, for the lines under it: shown only
/// while nothing is filtered, since they name no row.
fn notes(grants: &RoleGrants, left_out: usize) -> Vec<String> {
    let beyond = &grants.beyond;
    let mut notes = Vec::new();
    if !beyond.predefined.is_empty() {
        notes.push(format!(
            "Holds {}, which reach past any grant listed.",
            beyond.predefined.join(", ")
        ));
    }
    if beyond.defaults > 0 {
        notes.push(format!(
            "{} of ALTER DEFAULT PRIVILEGES name it: they decide what its future objects hold.",
            plural(beyond.defaults as usize, "rule")
        ));
    }
    if left_out > 0 {
        notes.push(format!(
            "{} it holds nothing on left out.",
            plural(left_out, "schema")
        ));
    }
    notes
}

/// A role's access, a row per database it can reach, with under each read
/// one for each schema it holds something on and each relation granted to it.
/// A relation is told apart by schema and name and not by their spelling
/// joined: `"a.b".c` and `a."b.c"` are two rows. Each cell takes the
/// strongest of its marks ([`Mark::merge`]), so a grant of its own wins over
/// the same one held through a role, and one a grantor made grantable over
/// one another did not. A relation in a schema without `USAGE` is marked
/// blocked, whoever granted it. `filter` keeps the rows whose name it
/// matches, through the one fuzzy matcher, with the database and schema
/// they are listed under.
pub(crate) fn role_rows(view: &RoleView, columns: &[String], filter: &str) -> Vec<MatrixRow> {
    let query = Query::new(filter);
    let mut fuzzy = Fuzzy::new();
    let mut matches = |name: &str| fuzzy.matches(&query, name);
    let filtering = !filter.trim().is_empty();
    let mut rows = Vec::new();
    for section in &view.sections {
        let header = database_row(section, columns);
        let Read::Done(grants) = &section.read else {
            if matches(&header.name) {
                rows.push(header);
            }
            continue;
        };
        let database = section.access.name.as_str();
        let mut inside: Vec<MatrixRow> = Vec::new();
        let mut left_out = 0;
        let mut schemas: Vec<&str> = grants.schemas.iter().map(|s| s.name.as_str()).collect();
        for held in &grants.held {
            if !schemas.contains(&held.schema.as_str()) {
                schemas.push(&held.schema);
            }
        }
        for name in schemas {
            let relations: Vec<&HeldGrant> = (grants.held.iter())
                .filter(|held| held.schema == name)
                .collect();
            let access = grants.schemas.iter().find(|schema| schema.name == name);
            if relations.is_empty()
                && access.is_none_or(|schema| schema.rights.is_empty() && schema.owns == 0)
            {
                left_out += 1;
                continue;
            }
            let kept = relation_rows(database, &relations, columns, &mut matches);
            let mut row = access
                .map(|schema| schema_row(database, schema, columns))
                .unwrap_or_else(|| schema_row(database, &closed(name), columns));
            if !kept.is_empty() || matches(&row.name) {
                row.opened = !kept.is_empty();
                inside.push(row);
                inside.extend(kept);
            }
        }
        if !filtering {
            let lines = notes(grants, left_out);
            inside.extend(
                (lines.into_iter().enumerate()).map(|(index, line)| note(database, index, line)),
            );
        }
        if matches(&header.name) || !inside.is_empty() {
            rows.push(header);
            rows.extend(inside);
        }
    }
    rows
}

/// A schema a grant names that the schema read did not list.
fn closed(name: &str) -> SchemaAccess {
    SchemaAccess {
        name: name.to_string(),
        rights: Vec::new(),
        owns: 0,
    }
}

/// The rows of one schema's relations.
fn relation_rows(
    database: &str,
    held: &[&HeldGrant],
    columns: &[String],
    matches: &mut impl FnMut(&str) -> bool,
) -> Vec<MatrixRow> {
    let mut rows: Vec<MatrixRow> = Vec::new();
    let mut through_of: Vec<Vec<String>> = Vec::new();
    let mut usable: Vec<bool> = Vec::new();
    let mut placed: HashMap<(&str, &str), usize> = HashMap::new();
    for grant in held {
        let key = (grant.schema.as_str(), grant.relation.as_str());
        let at = match placed.get(&key) {
            Some(at) => *at,
            None => {
                let full = format!("{}.{}", grant.schema, grant.relation);
                if !matches(&full) {
                    continue;
                }
                placed.insert(key, rows.len());
                rows.push(MatrixRow {
                    key: format!("{database}\u{1f}{}\u{1f}{}", grant.schema, grant.relation),
                    name: grant.relation.clone(),
                    via: String::new(),
                    who: Who::Object,
                    depth: 2,
                    member: false,
                    opened: false,
                    marks: vec![Mark::None; columns.len()],
                });
                through_of.push(Vec::new());
                usable.push(grant.usable);
                rows.len() - 1
            }
        };
        let Some(column) = columns.iter().position(|column| *column == grant.privilege) else {
            continue;
        };
        let mark = match (grant.through.as_deref(), grant.grantable) {
            (Some(role), _) => {
                if !through_of[at].iter().any(|through| through == role) {
                    through_of[at].push(role.to_string());
                }
                match role {
                    "PUBLIC" => Mark::Public,
                    _ => Mark::Through,
                }
            }
            (None, true) => Mark::Grantable,
            (None, false) => Mark::Granted,
        };
        let cell = &mut rows[at].marks[column];
        *cell = cell.merge(mark);
    }
    for ((row, through), usable) in rows.iter_mut().zip(through_of).zip(usable) {
        let direct = row
            .marks
            .iter()
            .any(|mark| matches!(mark, Mark::Granted | Mark::Grantable));
        let mut via = match (direct, through.is_empty()) {
            (true, true) => "direct".to_string(),
            (false, false) => format!("through {}", through.join(", ")),
            (true, false) => format!("direct, and through {}", through.join(", ")),
            (false, true) => String::new(),
        };
        if !usable {
            // The grant is real and reaches nothing: say which of the two it
            // is, not just that a mark is missing.
            via = format!("{via} · no USAGE on {}, so out of reach", held_schema(held));
            for mark in &mut row.marks {
                if *mark != Mark::None {
                    *mark = Mark::Blocked;
                }
            }
        }
        row.via = via;
    }
    rows
}

fn held_schema<'a>(held: &[&'a HeldGrant]) -> &'a str {
    held.first().map_or("", |grant| grant.schema.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold_row(key: &str, depth: u8, member: bool, opened: bool) -> MatrixRow {
        MatrixRow {
            key: key.into(),
            name: key.into(),
            via: String::new(),
            who: Who::Schema,
            depth,
            member,
            opened,
            marks: Vec::new(),
        }
    }

    #[test]
    fn a_collapsed_row_hides_what_is_under_it_and_nothing_after() {
        let rows = vec![
            fold_row("db1", 0, false, true),
            fold_row("db1.s1", 1, false, true),
            fold_row("db1.s1.t1", 2, false, false),
            fold_row("db1.s2", 1, false, true),
            fold_row("db1.s2.t1", 2, false, false),
            fold_row("db2", 0, false, true),
            fold_row("db2.s1", 1, false, false),
        ];
        let keys = |collapsed: &[&str]| -> Vec<String> {
            let collapsed = collapsed.iter().map(|key| key.to_string()).collect();
            folded(&rows, &collapsed)
                .into_iter()
                .map(|row| row.key)
                .collect()
        };
        assert_eq!(keys(&[]).len(), rows.len());
        assert_eq!(
            keys(&["db1.s1"]),
            ["db1", "db1.s1", "db1.s2", "db1.s2.t1", "db2", "db2.s1"]
        );
        assert_eq!(keys(&["db1"]), ["db1", "db2", "db2.s1"]);
        // A fold inside a folded row stays folded once it shows again.
        assert_eq!(keys(&["db1", "db1.s1"]), ["db1", "db2", "db2.s1"]);
        // A row with nothing under it does not fold what follows.
        assert_eq!(keys(&["db2.s1"]).len(), rows.len());
    }

    #[test]
    fn a_collapsed_role_hides_its_members_and_not_the_next_role() {
        let rows = vec![
            fold_row("a", 0, false, true),
            fold_row("a/m1", 0, true, false),
            fold_row("a/m2", 0, true, false),
            fold_row("b", 0, false, false),
        ];
        let collapsed = HashSet::from(["a".to_string()]);
        let keys: Vec<String> = folded(&rows, &collapsed)
            .into_iter()
            .map(|row| row.key)
            .collect();
        assert_eq!(keys, ["a", "b"]);
    }
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
            usable: true,
        };
        let view = view_of(RoleGrants {
            database: "d".into(),
            held: vec![held("a.b", "c"), held("a", "b.c")],
            ..RoleGrants::default()
        });
        let rows = relations(&view, "");
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].key, rows[1].key);
    }

    fn grant_on(relation: &str, privilege: &str, through: Option<&str>) -> HeldGrant {
        HeldGrant {
            schema: "public".into(),
            relation: relation.into(),
            privilege: privilege.into(),
            grantable: false,
            through: through.map(str::to_string),
            usable: true,
        }
    }

    /// One database, "d", read, holding `grants`.
    fn view_of(grants: RoleGrants) -> RoleView {
        let mut view = RoleView::plan(
            RoleAccess {
                superuser: false,
                databases: vec![database("d", Some(Source::Public))],
            },
            |_| true,
        );
        view.land("d", Ok(grants));
        view
    }

    /// The relation rows alone.
    fn relations(view: &RoleView, filter: &str) -> Vec<MatrixRow> {
        role_rows(view, &role_columns(view), filter)
            .into_iter()
            .filter(|row| row.who == Who::Object)
            .collect()
    }

    #[test]
    fn a_privilege_from_two_grantors_shows_the_stronger_mark_in_either_order() {
        let held = |grantable: bool| HeldGrant {
            usable: true,
            grantable,
            ..grant_on("orders", "SELECT", None)
        };
        for order in [[false, true], [true, false]] {
            let view = view_of(RoleGrants {
                held: order.into_iter().map(held).collect(),
                ..RoleGrants::default()
            });
            assert_eq!(
                relations(&view, "")[0].marks,
                [Mark::None, Mark::Grantable],
                "{order:?}"
            );
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
        let view = view_of(RoleGrants {
            held: vec![HeldGrant {
                schema: "sales".into(),
                ..grant_on("orders", "SELECT", None)
            }],
            ..RoleGrants::default()
        });
        assert_eq!(names(relations(&view, "slsord")), ["orders"]);
        // The schema and database stay with the relation that matched.
        let kept = names(role_rows(&view, &role_columns(&view), "slsord"));
        assert_eq!(kept, ["d", "sales", "orders"]);
        assert!(role_rows(&view, &role_columns(&view), "zzz").is_empty());
    }

    #[test]
    fn a_roles_own_grant_wins_over_one_held_through_a_role() {
        let view = view_of(RoleGrants {
            held: vec![
                grant_on("orders", "SELECT", None),
                grant_on("orders", "SELECT", Some("reporting")),
                grant_on("orders", "UPDATE", Some("app_rw")),
            ],
            ..RoleGrants::default()
        });
        let columns = role_columns(&view);
        assert_eq!(columns, ["CONNECT", "SELECT", "UPDATE"]);
        let rows = relations(&view, "");
        assert_eq!(rows[0].marks, [Mark::None, Mark::Granted, Mark::Through]);
        assert_eq!(rows[0].via, "direct, and through reporting, app_rw");
    }

    fn database(name: &str, connect: Option<Source>) -> DatabaseAccess {
        DatabaseAccess {
            name: name.into(),
            allows_connections: true,
            rights: connect
                .into_iter()
                .map(|source| Right {
                    privilege: "CONNECT".into(),
                    sources: vec![source],
                })
                .collect(),
        }
    }

    #[test]
    fn only_databases_it_can_connect_to_and_the_filter_shows_are_read() {
        let mut closed = database("closed", Some(Source::Public));
        closed.allows_connections = false;
        let access = RoleAccess {
            superuser: false,
            databases: vec![
                database("app", Some(Source::Direct)),
                database("audit", None),
                database("hidden", Some(Source::Public)),
                closed,
                database("shop", Some(Source::Via("team".into()))),
            ],
        };
        let view = RoleView::plan(access, |name| name != "hidden");
        assert_eq!(view.pending(), ["app", "shop"]);
        let reads: Vec<&Read> = view.sections.iter().map(|section| &section.read).collect();
        assert_eq!(reads[1], &Read::Skipped(Skipped::NoConnect));
        assert_eq!(reads[2], &Read::Skipped(Skipped::Hidden));
        assert_eq!(reads[3], &Read::Skipped(Skipped::NoConnect));
        // Every one is listed, said why not read, none dropped.
        let rows = role_rows(&view, &role_columns(&view), "");
        let via = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .unwrap()
                .via
                .clone()
        };
        assert_eq!(via("audit"), "no CONNECT: not read");
        assert_eq!(via("closed"), "takes no connections: not read");
        assert!(via("hidden").starts_with("hidden by the filter: not read"));
        assert_eq!(via("app"), "reading… · connect direct");
    }

    #[test]
    fn a_superuser_is_listed_everywhere_it_connects_and_read_nowhere() {
        let access = RoleAccess {
            superuser: true,
            databases: vec![
                database("a", Some(Source::Direct)),
                database("b", Some(Source::Direct)),
            ],
        };
        let view = RoleView::plan(access, |_| true);
        assert!(view.pending().is_empty());
        assert!(
            view.sections
                .iter()
                .all(|section| section.read == Read::Skipped(Skipped::Superuser))
        );
    }

    #[test]
    fn no_more_than_the_limit_is_read() {
        let access = RoleAccess {
            superuser: false,
            databases: (0..MAX_DATABASES_READ + 3)
                .map(|n| database(&format!("db{n:02}"), Some(Source::Public)))
                .collect(),
        };
        let view = RoleView::plan(access, |_| true);
        assert_eq!(view.pending().len(), MAX_DATABASES_READ);
        assert_eq!(
            view.sections.last().map(|section| &section.read),
            Some(&Read::Skipped(Skipped::Limit))
        );
    }

    #[test]
    fn a_failed_database_is_its_own_row_and_the_others_still_land() {
        let access = RoleAccess {
            superuser: false,
            databases: vec![
                database("a", Some(Source::Public)),
                database("b", Some(Source::Public)),
            ],
        };
        let mut view = RoleView::plan(access, |_| true);
        view.land("a", Err("connection refused".into()));
        view.land("b", Ok(RoleGrants::default()));
        // A late answer for a database already answered changes nothing.
        view.land("b", Err("late".into()));
        let rows = role_rows(&view, &role_columns(&view), "");
        assert_eq!(
            rows[0].via,
            "could not read: connection refused · connect PUBLIC"
        );
        assert_eq!(rows[1].via, "connect PUBLIC");
        assert!(view.pending().is_empty());
    }

    fn schema(name: &str, rights: &[(&str, Source)], owns: u64) -> SchemaAccess {
        SchemaAccess {
            name: name.into(),
            rights: rights
                .iter()
                .map(|(privilege, source)| Right {
                    privilege: privilege.to_string(),
                    sources: vec![source.clone()],
                })
                .collect(),
            owns,
        }
    }

    #[test]
    fn a_grant_in_a_schema_without_usage_is_blocked_and_says_so() {
        let mut view = RoleView::plan(
            RoleAccess {
                superuser: false,
                databases: vec![database("shop", Some(Source::Public))],
            },
            |_| true,
        );
        view.land(
            "shop",
            Ok(RoleGrants {
                database: "shop".into(),
                schemas: vec![
                    schema("sales", &[], 0),
                    schema("open", &[("USAGE", Source::Public)], 0),
                    schema("empty", &[], 0),
                ],
                held: vec![
                    HeldGrant {
                        schema: "sales".into(),
                        usable: false,
                        ..grant_on("orders", "SELECT", None)
                    },
                    HeldGrant {
                        schema: "open".into(),
                        ..grant_on("items", "SELECT", Some("PUBLIC"))
                    },
                ],
                ..RoleGrants::default()
            }),
        );
        let columns = role_columns(&view);
        assert_eq!(columns, ["CONNECT", "USAGE", "SELECT"]);
        let rows = role_rows(&view, &columns, "");
        let row = |name: &str| rows.iter().find(|row| row.name == name).unwrap();
        assert_eq!(row("orders").marks, [Mark::None, Mark::None, Mark::Blocked]);
        assert_eq!(
            row("orders").via,
            "direct · no USAGE on sales, so out of reach"
        );
        assert_eq!(row("items").marks, [Mark::None, Mark::None, Mark::Public]);
        assert_eq!(row("open").marks, [Mark::None, Mark::Public, Mark::None]);
        assert_eq!(row("sales").via, "no USAGE");
        assert_eq!(row("shop").marks, [Mark::Public, Mark::None, Mark::None]);
        // A schema it holds nothing on is counted, not listed.
        assert!(rows.iter().all(|row| row.name != "empty"));
        assert!(rows.iter().any(
            |row| row.who == Who::Note && row.name == "1 schema it holds nothing on left out."
        ));
        let mut keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "{keys:?}");
    }

    #[test]
    fn what_a_role_owns_and_inherits_is_said_where_no_grant_shows_it() {
        use crate::db::roles::Beyond;
        let mut view = RoleView::plan(
            RoleAccess {
                superuser: false,
                databases: vec![database("shop", Some(Source::Owner(None)))],
            },
            |_| true,
        );
        view.land(
            "shop",
            Ok(RoleGrants {
                schemas: vec![schema(
                    "public",
                    &[("USAGE", Source::Owner(Some("owners".into())))],
                    3,
                )],
                beyond: Beyond {
                    predefined: vec!["pg_read_all_data".into()],
                    defaults: 2,
                    ..Beyond::default()
                },
                ..RoleGrants::default()
            }),
        );
        let rows = role_rows(&view, &role_columns(&view), "");
        let public = rows.iter().find(|row| row.name == "public").unwrap();
        assert_eq!(public.via, "usage owner, as owners · owns 3 relations");
        let notes: Vec<&str> = (rows.iter())
            .filter(|row| row.who == Who::Note)
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            notes,
            [
                "Holds pg_read_all_data, which reach past any grant listed.",
                "2 rules of ALTER DEFAULT PRIVILEGES name it: they decide what its future objects hold."
            ]
        );
        // Filtering names rows, and a note is none.
        assert!(
            role_rows(&view, &role_columns(&view), "public")
                .iter()
                .all(|row| row.who != Who::Note)
        );
    }
}
