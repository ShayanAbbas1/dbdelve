//! A server's logins and roles, who holds which grant on an object, and the
//! statements that change a role, as plain data.
//!
//! Read only where [`super::Target::manages_roles`]: Postgres today. What
//! crosses out of `src/db/` is the rendered names and flags below, never a
//! catalog row. What the server does not record is `None`, never a guess:
//! Postgres keeps no last login anywhere a client can read.

use super::{DbError, Engine, QueryResult, optional_cell, plain_error, required_cell};

/// One login or role of the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub login: bool,
    pub superuser: bool,
    /// When its password stops being accepted, `YYYY-MM-DD`; `None` for never.
    pub password_expires: Option<String>,
    /// Whether that date has passed.
    pub expired: bool,
    /// `None` for no limit.
    pub connection_limit: Option<i64>,
    /// Whether it has a password set, where the connection may read that:
    /// `None` when it may not, which is every role but a superuser's.
    pub has_password: Option<bool>,
    /// When it last logged in, where the server records it.
    pub last_login: Option<String>,
}

impl Role {
    /// Whether this is a login that was turned off rather than a role made
    /// to be granted: one that cannot log in but still has a password. Only
    /// knowable where the password is readable.
    pub fn deactivated(&self) -> bool {
        !self.login && self.has_password == Some(true)
    }
}

/// Every role of the server and who is a member of whom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub roles: Vec<Role>,
    /// `(member, role)`: `member` belongs to `role`, directly.
    pub memberships: Vec<(String, String)>,
    /// Whether members take on the privileges of the roles they belong to,
    /// by member then role. A membership missing here inherits.
    pub not_inherited: Vec<(String, String)>,
    /// Whether the server records logins at all, so a missing
    /// [`Role::last_login`] means never rather than unknown.
    pub records_logins: bool,
}

impl Roles {
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|role| role.name == name)
    }

    /// The roles `name` belongs to directly, in name order.
    pub fn member_of(&self, name: &str) -> Vec<&str> {
        let mut of: Vec<&str> = (self.memberships.iter())
            .filter(|(member, _)| member == name)
            .map(|(_, role)| role.as_str())
            .collect();
        of.sort_unstable();
        of
    }

    /// The direct members of `name`, in name order.
    pub fn members(&self, name: &str) -> Vec<&str> {
        let mut members: Vec<&str> = (self.memberships.iter())
            .filter(|(_, role)| role == name)
            .map(|(member, _)| member.as_str())
            .collect();
        members.sort_unstable();
        members
    }

    /// Whether `member` takes on `role`'s privileges through its membership.
    pub fn inherits(&self, member: &str, role: &str) -> bool {
        !(self.not_inherited.iter()).any(|(m, r)| m == member && r == role)
    }

    /// Why a new role may not be made a member of `name`: it is a
    /// superuser or [`privileged_name`], or belongs to one, however far up.
    /// Inheriting or not: a member can `SET ROLE` to what it belongs to.
    pub fn refuses_membership(&self, name: &str) -> Option<String> {
        let mut seen = vec![name];
        let mut index = 0;
        while let Some(&role) = seen.get(index) {
            index += 1;
            let reason = match self.role(role) {
                _ if privileged_name(role) => "a privileged role",
                Some(found) if found.superuser => "a superuser",
                _ => {
                    for (member, of) in &self.memberships {
                        if member == role && !seen.contains(&of.as_str()) {
                            seen.push(of);
                        }
                    }
                    continue;
                }
            };
            return Some(match role == name {
                true => format!("{name} is {reason}; a role made here is never a member of one."),
                false => format!(
                    "{name} belongs to {role}, {reason}; a role made here is never a member of one."
                ),
            });
        }
        None
    }
}

/// The predefined roles a new role is never made a member of from here:
/// each reaches past the database's own grants, to the server's files and
/// programs (`pg_execute_server_program`, `pg_read_server_files`,
/// `pg_write_server_files`, any one a way to superuser), to other sessions
/// (`pg_signal_backend`, `pg_signal_autovacuum_worker`), to the server's
/// own work (`pg_checkpoint`, `pg_create_subscription`), or to every table
/// a superuser's code trusts (`pg_write_all_data`).
pub const PRIVILEGED_ROLES: &[&str] = &[
    "pg_execute_server_program",
    "pg_read_server_files",
    "pg_write_server_files",
    "pg_signal_backend",
    "pg_signal_autovacuum_worker",
    "pg_checkpoint",
    "pg_create_subscription",
    "pg_write_all_data",
];

/// Whether `IN ROLE` refuses `name` whatever the server says of it: the
/// bootstrap superuser `postgres`, and [`PRIVILEGED_ROLES`].
pub fn privileged_name(name: &str) -> bool {
    name == "postgres" || PRIVILEGED_ROLES.contains(&name)
}

/// One privilege held on an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// `None` for PUBLIC, every role.
    pub grantee: Option<String>,
    /// As the server spells it, `SELECT`.
    pub privilege: String,
    /// Whether the grantee may grant it on.
    pub grantable: bool,
}

/// Who holds what on one object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectGrants {
    pub owner: String,
    /// The privileges an object of this kind can be granted, in the order the
    /// dialog lists them.
    pub privileges: Vec<String>,
    pub grants: Vec<Grant>,
    pub roles: Roles,
    /// A sequence, which a grant names `ON SEQUENCE`.
    pub sequence: bool,
}

/// Where a privilege a role holds comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The role is a superuser, which bypasses every check.
    Superuser,
    /// It owns the object, itself (`None`) or as a member of the owner it
    /// inherits from.
    Owner(Option<String>),
    /// A grant to the role itself.
    Direct,
    /// A grant to a role it inherits from: the grantee, not every role on
    /// the way to it.
    Via(String),
    Public,
    /// A predefined role it holds (`pg_read_all_data`), which reaches past
    /// the object's own list.
    Predefined(String),
    /// Held, though no grant on the object says so: the owner of the
    /// database, which Postgres 14 and later make owner of `public`, or a
    /// predefined role.
    Implicit,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::Superuser => "superuser".into(),
            Self::Owner(None) => "owner".into(),
            Self::Owner(Some(role)) => format!("owner, as {role}"),
            Self::Direct => "direct".into(),
            Self::Via(role) => format!("through {role}"),
            Self::Public => "PUBLIC".into(),
            Self::Predefined(roles) => roles.clone(),
            Self::Implicit => "database owner or a predefined role".into(),
        }
    }

    /// Strongest first: what a role holds of its own before what it inherits,
    /// and PUBLIC, which every role holds, last.
    fn rank(&self) -> u8 {
        match self {
            Self::Superuser => 0,
            Self::Owner(_) => 1,
            Self::Direct => 2,
            Self::Via(_) => 3,
            Self::Predefined(_) => 4,
            Self::Public => 5,
            Self::Implicit => 6,
        }
    }
}

/// A privilege the role holds on a database or a schema, with where each
/// way of holding it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Right {
    /// As the server spells it: `CONNECT`, `USAGE`, `CREATE`, `TEMPORARY`.
    pub privilege: String,
    pub sources: Vec<Source>,
}

/// What a role can do to one database of the server, from the catalogs every
/// database shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseAccess {
    pub name: String,
    /// `datallowconn`: a database that takes no connections cannot be
    /// connected to whatever is granted on it.
    pub allows_connections: bool,
    /// The database privileges the role holds, effective ones only.
    pub rights: Vec<Right>,
}

impl DatabaseAccess {
    /// Whether the role can connect to it, which is what lets anything
    /// inside it be reached.
    pub fn connects(&self) -> bool {
        self.allows_connections && self.rights.iter().any(|right| right.privilege == "CONNECT")
    }
}

/// A role's access to every database of the server, read in the connected
/// one: `pg_database` and the role catalogs are shared by all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleAccess {
    pub superuser: bool,
    pub databases: Vec<DatabaseAccess>,
}

/// What a role holds on one schema of a database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaAccess {
    pub name: String,
    /// `USAGE` and `CREATE`, effective ones only.
    pub rights: Vec<Right>,
    /// How many of its relations the role owns, itself or through a role it
    /// inherits.
    pub owns: u64,
}

impl SchemaAccess {
    pub fn usable(&self) -> bool {
        self.rights.iter().any(|right| right.privilege == "USAGE")
    }
}

/// Every privilege one role holds in one database, itself or through a role
/// it belongs to or PUBLIC: on the database's schemas and relations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleGrants {
    pub database: String,
    pub schemas: Vec<SchemaAccess>,
    pub held: Vec<HeldGrant>,
    /// What reaches past the grants listed, for a list that is empty or
    /// short: a superuser bypasses them, an owner holds everything on what
    /// it owns, which is left out, and a predefined role reaches further.
    pub beyond: Beyond,
}

/// What a role holds past its grants on relations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Beyond {
    pub superuser: bool,
    /// How many relations it owns, itself or through a role it inherits.
    pub owns: u64,
    /// The predefined roles it holds (`pg_read_all_data` and the like).
    pub predefined: Vec<String>,
    /// How many default-privilege rules of the database (`ALTER DEFAULT
    /// PRIVILEGES`) it sets or is granted by, which decide what its future
    /// objects hold and which no grant listed shows.
    pub defaults: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldGrant {
    pub schema: String,
    pub relation: String,
    /// As the server spells it, `SELECT`.
    pub privilege: String,
    pub grantable: bool,
    /// The role it is held through; `None` for the role's own grant,
    /// `PUBLIC` for a grant to every role.
    pub through: Option<String>,
    /// Whether the role holds `USAGE` on the schema: without it nothing in
    /// the schema is reached, whatever is granted on it.
    pub usable: bool,
    /// A sequence, which a grant names `ON SEQUENCE`: the `TABLE` form is
    /// refused for `USAGE`.
    pub sequence: bool,
}

/// What one role can reach in the connection's database, for the detail
/// panel's summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    pub database: String,
    /// One per schema and privilege, in schema then privilege order.
    pub entries: Vec<ReachEntry>,
    /// How many relations it owns, itself or through a role it inherits.
    pub owns: u64,
    /// The predefined roles it holds (`pg_read_all_data` and the like),
    /// which reach further than any grant listed.
    pub predefined: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachEntry {
    pub schema: String,
    pub privilege: String,
    pub relations: u64,
    /// The roles it holds them through, itself left out.
    pub through: Vec<String>,
}

/// The server's audit lists, each read from its own catalogs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reports {
    pub superusers: Vec<String>,
    /// Logins that could connect and have not in 90 days, where the server
    /// records logins; `None` where it does not.
    pub idle: Option<Vec<String>>,
    pub expired: Vec<String>,
    /// `schema.name` of every relation PUBLIC holds a privilege on.
    pub public_grants: Vec<String>,
    pub unused_roles: Vec<String>,
    /// Sessions open now, by login, most first.
    pub sessions: Vec<(String, u64)>,
}

/// A role to create. Its `Debug` leaves the password out, so a log or a
/// panic message never carries it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct NewRole {
    pub name: String,
    pub login: bool,
    pub password: Option<String>,
    pub connection_limit: Option<i64>,
    /// `YYYY-MM-DD`.
    pub password_expires: Option<String>,
    pub member_of: Vec<String>,
}

impl std::fmt::Debug for NewRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewRole")
            .field("name", &self.name)
            .field("login", &self.login)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("connection_limit", &self.connection_limit)
            .field("password_expires", &self.password_expires)
            .field("member_of", &self.member_of)
            .finish()
    }
}

/// What Edit changes about a role.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleEdit {
    pub name: String,
    pub connection_limit: Option<i64>,
    pub password_expires: Option<String>,
}

/// The statements that make `role`, one per line.
pub fn create_role(role: &NewRole) -> String {
    let mut statement = format!(
        "CREATE ROLE {} {}",
        ident(&role.name),
        if role.login { "LOGIN" } else { "NOLOGIN" }
    );
    if let Some(password) = &role.password {
        statement.push_str(&format!(" PASSWORD {}", verifier(password)));
    }
    if let Some(limit) = role.connection_limit {
        statement.push_str(&format!(" CONNECTION LIMIT {limit}"));
    }
    if let Some(date) = &role.password_expires {
        statement.push_str(&format!(" VALID UNTIL {}", literal(date)));
    }
    if !role.member_of.is_empty() {
        let roles: Vec<String> = role.member_of.iter().map(|name| ident(name)).collect();
        statement.push_str(&format!(" IN ROLE {}", roles.join(", ")));
    }
    format!("{statement};")
}

/// The statement Edit runs, `None` when nothing changes. A limit can be
/// set or changed and not cleared: Postgres clears one with `CONNECTION
/// LIMIT -1`, which sqlparser 0.63 cannot read, so the mode check could not
/// either; [`clears_limit`] is how a caller finds that out first.
///
/// ponytail: membership is set when a role is created (`IN ROLE`) and not
/// edited here. `GRANT role TO member` is the only way to change it, and
/// sqlparser 0.63 cannot read one, so the mode check would take it for an
/// unreadable statement and run it on a confirmation instead of asking for
/// Full. The upgrade is a parser that reads it, then `GRANT`/`REVOKE` here and
/// in `sql::is_generated_role_change`.
pub fn edit_role(before: &Role, edit: &RoleEdit) -> Option<String> {
    let mut statements = Vec::new();
    let mut options = Vec::new();
    if let Some(limit) = edit
        .connection_limit
        .filter(|_| edit.connection_limit != before.connection_limit)
    {
        options.push(format!("CONNECTION LIMIT {limit}"));
    }
    if edit.password_expires != before.password_expires {
        let date = edit.password_expires.as_deref().unwrap_or("infinity");
        options.push(format!("VALID UNTIL {}", literal(date)));
    }
    if !options.is_empty() {
        statements.push(format!(
            "ALTER ROLE {} WITH {};",
            ident(&edit.name),
            options.join(" ")
        ));
    }
    (!statements.is_empty()).then(|| statements.join("\n"))
}

/// Whether `edit` would take away the limit `before` has, which Edit does
/// not write.
pub fn clears_limit(before: &Role, edit: &RoleEdit) -> bool {
    before.connection_limit.is_some() && edit.connection_limit.is_none()
}

/// A new password, sent as its SCRAM verifier.
pub fn reset_password(name: &str, password: &str) -> String {
    format!(
        "ALTER ROLE {} WITH PASSWORD {};",
        ident(name),
        verifier(password)
    )
}

/// Turn a login off, or back on.
pub fn set_login(name: &str, login: bool) -> String {
    format!(
        "ALTER ROLE {} WITH {};",
        ident(name),
        if login { "LOGIN" } else { "NOLOGIN" }
    )
}

/// The statements that drop `role`, for Copy DROP ROLE: text for the
/// clipboard, never sent or put in a buffer (AGENTS.md, hard rule 1), as the
/// explorer's Copy DROP is. `REASSIGN OWNED` and `DROP OWNED` act on the
/// current database only, so the note names `databases` (what the profile
/// lists, which the include filter has already narrowed) to run them in.
/// `connected` is the role the connection signs in as, offered to take over
/// what `role` owns; with none known, or when it is `role` itself, a marked
/// placeholder stands in. A connected role or a superuser still gets the
/// text, with a warning line. Names in a comment lose their line breaks, so
/// a name cannot end the comment.
pub fn drop_role_text(role: &Role, connected: Option<&str>, databases: &[String]) -> String {
    let flat = |text: &str| text.replace(['\n', '\r'], " ");
    let name = ident(&role.name);
    let named = flat(&name);
    let itself = connected == Some(role.name.as_str());
    let heir = connected.filter(|_| !itself);
    let mut lines = vec![format!(
        "-- REASSIGN OWNED and DROP OWNED act on the current database only: run them in every database where {named} owns objects or holds privileges{}.",
        match databases.is_empty() {
            true => String::new(),
            false => format!(": {}", flat(&databases.join(", "))),
        }
    )];
    lines.push(match heir {
        Some(heir) => format!(
            "-- Check that {} is the role that should take ownership of what {named} owns.",
            flat(&ident(heir))
        ),
        None => format!(
            "-- Replace <new_owner> with the role that should take ownership of what {named} owns."
        ),
    });
    if itself {
        lines.push(format!(
            "-- Warning: {named} is the role this connection is signed in as, and the server refuses to drop the current user. Connect as another role first."
        ));
    }
    if role.superuser {
        lines.push(format!(
            "-- Warning: {named} is a superuser: dropping it removes everything it can do."
        ));
    }
    lines.push(format!(
        "REASSIGN OWNED BY {name} TO {};",
        heir.map_or_else(|| "<new_owner>".to_string(), ident)
    ));
    lines.push(format!("DROP OWNED BY {name};"));
    lines.push(format!("DROP ROLE {name};"));
    lines.join("\n")
}

fn ident(name: &str) -> String {
    Engine::Postgres.quote_identifier(name)
}

/// What a grant is on. A relation says whether it is a sequence because
/// Postgres refuses `USAGE` through the `TABLE` form.
#[allow(dead_code)] // read by the Show Grants editor, next commit
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GrantObject {
    Database(String),
    Schema(String),
    Relation {
        schema: String,
        name: String,
        sequence: bool,
    },
}

#[allow(dead_code)] // read by the Show Grants editor, next commit
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GrantAction {
    Grant,
    Revoke,
}

/// One privilege to grant to or revoke from one role on one object.
///
/// Grant option is out of scope: a grant is written without `WITH GRANT
/// OPTION`, and a revoke of a direct grant takes its grant option with it,
/// as Postgres does. A revoke never says `CASCADE`, so a grant others
/// re-granted fails with the server's own error, which is shown.
#[allow(dead_code)] // read by the Show Grants editor, next commit
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GrantChange {
    /// The database whose session runs it; `None` for the connection's own.
    pub database: Option<String>,
    pub object: GrantObject,
    pub grantee: String,
    /// As the server spells it: `SELECT`, `TEMPORARY`, ...
    pub privilege: String,
    pub action: GrantAction,
}

/// The statements of one database, run on that database's session.
#[allow(dead_code)] // read by the Show Grants editor, next commit
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantBatch {
    pub database: Option<String>,
    pub sql: String,
}

/// The order a table's privileges are listed in, those of a database and a
/// schema before them. Statements spell them in it too.
pub const PRIVILEGE_ORDER: [&str; 11] = [
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

/// Whether `privilege` is one a grant on `object` may name. This is the set
/// `sql::is_generated_grant_change` admits, and no other: never `ALL`, and
/// not `MAINTAIN`, which sqlparser 0.63 cannot read.
#[allow(dead_code)] // read by the Show Grants editor, next commit
pub fn grant_applies(object: &GrantObject, privilege: &str) -> bool {
    let takes: &[&str] = match object {
        GrantObject::Database(_) => &["CONNECT", "TEMPORARY", "CREATE"],
        GrantObject::Schema(_) => &["USAGE", "CREATE"],
        GrantObject::Relation { sequence: true, .. } => &["USAGE", "SELECT", "UPDATE"],
        GrantObject::Relation { .. } => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ],
    };
    takes.contains(&privilege)
}

/// The statements for `changes`, one batch per database (the connection's
/// own first), each statement on its own line: one per object, role and
/// action, its privileges in [`PRIVILEGE_ORDER`]. Revokes come before grants.
/// A privilege its object cannot take is dropped, never written, and a
/// grantee is always a role's name, never `PUBLIC`.
#[allow(dead_code)] // read by the Show Grants editor, next commit
pub fn grant_statements(changes: &[GrantChange]) -> Vec<GrantBatch> {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<_, Vec<&str>> = BTreeMap::new();
    for change in changes {
        if !grant_applies(&change.object, &change.privilege) {
            continue;
        }
        let key = (
            &change.database,
            change.action == GrantAction::Grant,
            &change.object,
            &change.grantee,
            change.action,
        );
        grouped.entry(key).or_default().push(&change.privilege);
    }
    let mut batches: Vec<GrantBatch> = Vec::new();
    for ((database, _, object, grantee, action), mut privileges) in grouped {
        privileges
            .sort_by_key(|privilege| PRIVILEGE_ORDER.iter().position(|known| known == privilege));
        privileges.dedup();
        let target = match object {
            GrantObject::Database(name) => format!("DATABASE {}", ident(name)),
            GrantObject::Schema(name) => format!("SCHEMA {}", ident(name)),
            GrantObject::Relation {
                schema,
                name,
                sequence,
            } => format!(
                "{} {}.{}",
                if *sequence { "SEQUENCE" } else { "TABLE" },
                ident(schema),
                ident(name)
            ),
        };
        let privileges = privileges.join(", ");
        let grantee = ident(grantee);
        let statement = match action {
            GrantAction::Grant => format!("GRANT {privileges} ON {target} TO {grantee};"),
            GrantAction::Revoke => format!("REVOKE {privileges} ON {target} FROM {grantee};"),
        };
        match batches.last_mut() {
            Some(batch) if batch.database == *database => {
                batch.sql.push('\n');
                batch.sql.push_str(&statement);
            }
            _ => batches.push(GrantBatch {
                database: database.clone(),
                sql: statement,
            }),
        }
    }
    batches
}

/// Read alike whatever `standard_conforming_strings` is: a role name is
/// anyone's to pick, and a catalog read quoting one runs as the viewer.
fn literal(value: &str) -> String {
    super::postgres_literal(value)
}

/// What the server stores for `password`, computed here so the statement,
/// the review and the history hold the hash and never the password.
fn verifier(password: &str) -> String {
    literal(&postgres_protocol::password::scram_sha_256(
        password.as_bytes(),
    ))
}

/// The roles read, `pg_authid` when the connection may read passwords and
/// `pg_roles` otherwise, then who is a member of whom.
pub(super) const ROLE_COLUMNS: &str = "
    r.rolname AS name,
    CASE WHEN r.rolcanlogin THEN 'y' ELSE 'n' END AS login,
    CASE WHEN r.rolsuper THEN 'y' ELSE 'n' END AS superuser,
    CASE WHEN r.rolvaliduntil IS NULL OR NOT pg_catalog.isfinite(r.rolvaliduntil) THEN NULL
         ELSE pg_catalog.to_char(r.rolvaliduntil, 'YYYY-MM-DD') END AS expires,
    CASE WHEN r.rolvaliduntil < pg_catalog.now() THEN 'y' ELSE 'n' END AS expired,
    CASE WHEN r.rolconnlimit < 0 THEN NULL ELSE r.rolconnlimit::text END AS connection_limit";

pub(super) fn roles_sql(passwords: bool) -> String {
    let (from, password) = if passwords {
        (
            "pg_catalog.pg_authid",
            "CASE WHEN r.rolpassword IS NULL THEN 'n' ELSE 'y' END",
        )
    } else {
        ("pg_catalog.pg_roles", "NULL")
    };
    format!(
        "SELECT {ROLE_COLUMNS}, {password} AS has_password
         FROM {from} AS r
         WHERE r.rolname !~ '^pg_'
         ORDER BY r.rolcanlogin DESC, r.rolname"
    )
}

/// `inherit_option` is per membership from Postgres 16; before it a member
/// inherits unless it was made `NOINHERIT` itself.
pub(super) fn memberships_sql(per_membership: bool) -> String {
    let inherits = if per_membership {
        "m.inherit_option"
    } else {
        "member.rolinherit"
    };
    // From 16 one membership is a row per grantor, each with its own
    // inherit option: it inherits when any of them says so.
    format!(
        "SELECT member.rolname AS member, role.rolname AS role,
             CASE WHEN pg_catalog.bool_or({inherits}) THEN 'y' ELSE 'n' END AS inherits
         FROM pg_catalog.pg_auth_members AS m
         JOIN pg_catalog.pg_roles AS member ON member.oid = m.member
         JOIN pg_catalog.pg_roles AS role ON role.oid = m.roleid
         WHERE member.rolname !~ '^pg_'
         GROUP BY member.rolname, role.rolname"
    )
}

pub(super) const RELATION_GRANTS_SQL: &str = "
    SELECT pg_catalog.pg_get_userbyid(c.relowner) AS owner,
        c.relkind::text AS kind,
        CASE WHEN a.grantee = 0 THEN NULL ELSE pg_catalog.pg_get_userbyid(a.grantee) END AS grantee,
        a.privilege_type AS privilege,
        CASE WHEN a.is_grantable THEN 'y' ELSE 'n' END AS grantable
    FROM pg_catalog.pg_class AS c
    LEFT JOIN LATERAL pg_catalog.aclexplode(COALESCE(c.relacl,
        pg_catalog.acldefault(CASE WHEN c.relkind = 'S' THEN 's' ELSE 'r' END::\"char\", c.relowner))) AS a ON true
    WHERE c.relname = {relation}
      AND c.relnamespace = (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = {schema})";

/// The privileges each kind of relation takes, in the order Postgres's
/// `GRANT` documentation lists them. `MAINTAIN` joins from Postgres 17,
/// listed only when the server grants it somewhere on the object.
pub(super) fn privileges_of(kind: &str) -> &'static [&'static str] {
    match kind {
        "S" => &["USAGE", "SELECT", "UPDATE"],
        _ => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ],
    }
}

/// What separates names in one aggregated cell: no role name holds it.
pub(super) const SEPARATOR: char = '\u{1f}';

/// `held(oid, via)`: the role `name`, then every role it takes privileges
/// from, `via` the role each came through. A membership counts only where
/// it inherits: per membership from Postgres 16, by the member's own
/// `INHERIT` before it.
fn held(name: &str, per_membership: bool) -> String {
    let inherits = if per_membership {
        "m.inherit_option"
    } else {
        "member.rolinherit"
    };
    format!(
        "WITH RECURSIVE held(oid, via) AS (
             SELECT oid, NULL::name FROM pg_catalog.pg_roles WHERE rolname = {}
             UNION
             SELECT m.roleid, r.rolname
             FROM pg_catalog.pg_auth_members AS m
             JOIN held ON m.member = held.oid
             JOIN pg_catalog.pg_roles AS member ON member.oid = m.member
             JOIN pg_catalog.pg_roles AS r ON r.oid = m.roleid
             WHERE {inherits}
         ),
         holder(oid, via) AS (
             SELECT oid, via FROM held
             UNION ALL SELECT 0, 'PUBLIC'
         )",
        literal(name)
    )
}

/// Every privilege `name` holds on a relation, a sequence or some of a
/// relation's columns (`attacl`, said `SELECT (columns)`), itself or through
/// a role, each counted once per relation. Only in schemas it may use: a
/// grant on a relation in a schema it holds no `USAGE` on reaches nothing.
/// Owned relations are counted apart ([`owns_sql`]).
pub(super) fn reach_sql(name: &str, per_membership: bool) -> String {
    let usable = format!(
        "pg_catalog.has_schema_privilege({}, n.oid, 'USAGE')",
        literal(name)
    );
    format!(
        "{},
         granted(schema, privilege, relation, via) AS (
             SELECT n.nspname, a.privilege_type, c.oid, holder.via
             FROM pg_catalog.pg_class AS c
             JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
             CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
             JOIN holder ON holder.oid = a.grantee
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname !~ '^pg_toast'
               AND c.relowner NOT IN (SELECT oid FROM held)
               AND {usable}
             UNION ALL
             SELECT n.nspname, a.privilege_type || ' (columns)', c.oid, holder.via
             FROM pg_catalog.pg_attribute AS att
             JOIN pg_catalog.pg_class AS c ON c.oid = att.attrelid
             JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
             CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
             JOIN holder ON holder.oid = a.grantee
             WHERE att.attnum > 0 AND NOT att.attisdropped
               AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname !~ '^pg_toast'
               AND c.relowner NOT IN (SELECT oid FROM held)
               AND {usable}
         )
         SELECT schema, privilege,
             pg_catalog.count(DISTINCT relation)::text AS relations,
             pg_catalog.string_agg(DISTINCT via::text, E'\\x1f') AS through
         FROM granted
         GROUP BY 1, 2
         ORDER BY 1, 2",
        held(name, per_membership)
    )
}

/// What names a holder's way of holding a privilege in an aggregated cell:
/// `direct`, `public` or `via:` and the role the grant is on.
const SOURCE_TOKEN: &str = "CASE WHEN holder.oid = 0 THEN 'public' \
     WHEN holder.via IS NULL THEN 'direct' ELSE 'via:' || holder.via END";

/// The ways the role holds `privilege` on an object whose list is `acl`
/// (`acldefault` of its `owner` where there is none, which is what a NULL
/// list means): each holder's token, the owner's own entry left to
/// [`owner_sql`].
fn sources_sql(kind: char, acl: &str, owner: &str, privilege: &str) -> String {
    format!(
        "(SELECT pg_catalog.string_agg(DISTINCT {SOURCE_TOKEN}, E'\\x1f')
          FROM pg_catalog.aclexplode(
              COALESCE({acl}, pg_catalog.acldefault('{kind}', {owner}))) AS a
          JOIN holder ON holder.oid = a.grantee
          WHERE a.privilege_type = '{privilege}' AND a.grantee <> {owner})"
    )
}

/// Whether the role owns the object, itself or through a role it inherits.
fn owner_sql(owner: &str) -> String {
    format!(
        "(SELECT CASE WHEN held.via IS NULL THEN 'direct' ELSE 'via:' || held.via END
          FROM held WHERE held.oid = {owner} ORDER BY held.via NULLS FIRST LIMIT 1)"
    )
}

/// What the role holds of `privileges` on an object, as the columns
/// [`assemble_rights`] reads: whether each is effective (the server's own
/// `has_*_privilege`, which counts inheritance, PUBLIC, ownership, a
/// superuser and the predefined roles) and the grants that explain it.
fn rights_sql(
    name: &str,
    function: &str,
    object: &str,
    kind: char,
    acl: &str,
    owner: &str,
    privileges: &[&str],
) -> String {
    let mut columns: Vec<String> = privileges
        .iter()
        .flat_map(|privilege| {
            let column = privilege.to_lowercase();
            [
                format!(
                    "CASE WHEN pg_catalog.{function}({}, {object}, '{privilege}')
                     THEN 'y' ELSE 'n' END AS {column}",
                    literal(name)
                ),
                format!(
                    "{} AS {column}_via",
                    sources_sql(kind, acl, owner, privilege)
                ),
            ]
        })
        .collect();
    columns.push(format!("{} AS owner", owner_sql(owner)));
    columns.join(",\n             ")
}

const DATABASE_PRIVILEGES: [&str; 3] = ["CONNECT", "CREATE", "TEMPORARY"];
const SCHEMA_PRIVILEGES: [&str; 2] = ["USAGE", "CREATE"];

/// Every database of the server and what the role holds on it. The catalogs
/// it reads are shared, so any database answers for all of them.
pub(super) fn database_access_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT d.datname AS database,
             CASE WHEN d.datallowconn THEN 'y' ELSE 'n' END AS allows,
             (SELECT CASE WHEN r.rolsuper THEN 'y' ELSE 'n' END
              FROM pg_catalog.pg_roles AS r WHERE r.rolname = {}) AS superuser,
             {}
         FROM pg_catalog.pg_database AS d
         ORDER BY d.datname",
        held(name, per_membership),
        literal(name),
        rights_sql(
            name,
            "has_database_privilege",
            "d.oid",
            'd',
            "d.datacl",
            "d.datdba",
            &DATABASE_PRIVILEGES
        )
    )
}

/// The schemas of the connected database and what the role holds on each.
pub(super) fn schema_access_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT n.nspname AS schema,
             {},
             (SELECT pg_catalog.count(*)
              FROM pg_catalog.pg_class AS c
              WHERE c.relnamespace = n.oid
                AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
                AND c.relowner IN (SELECT oid FROM held))::text AS owns
         FROM pg_catalog.pg_namespace AS n
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname !~ '^pg_toast' AND n.nspname !~ '^pg_temp_'
         ORDER BY n.nspname",
        held(name, per_membership),
        rights_sql(
            name,
            "has_schema_privilege",
            "n.oid",
            'n',
            "n.nspacl",
            "n.nspowner",
            &SCHEMA_PRIVILEGES
        )
    )
}

/// Owned relations are left out: an owner holds everything on its own. A
/// relation in a schema the role cannot use is listed all the same, `usable`
/// saying it, because the grant is on it and a user looking for why a table
/// fails to open needs to see the grant is not the problem.
pub(super) fn role_grants_sql(name: &str, per_membership: bool) -> String {
    let usable = format!(
        "CASE WHEN pg_catalog.has_schema_privilege({}, n.oid, 'USAGE')
         THEN 'y' ELSE 'n' END",
        literal(name)
    );
    format!(
        "{held}
         SELECT n.nspname AS schema, c.relname AS relation, a.privilege_type AS privilege,
             CASE WHEN a.is_grantable THEN 'y' ELSE 'n' END AS grantable,
             holder.via AS through, pg_catalog.current_database() AS database,
             {usable} AS usable,
             CASE WHEN c.relkind = 'S' THEN 'y' ELSE 'n' END AS sequence
         FROM pg_catalog.pg_class AS c
         JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
         JOIN holder ON holder.oid = a.grantee
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND c.relowner NOT IN (SELECT oid FROM held)
         UNION ALL
         SELECT n.nspname, c.relname, a.privilege_type || ' (columns)',
             CASE WHEN a.is_grantable THEN 'y' ELSE 'n' END,
             holder.via, pg_catalog.current_database(), {usable}, 'n'
         FROM pg_catalog.pg_attribute AS att
         JOIN pg_catalog.pg_class AS c ON c.oid = att.attrelid
         JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) AS a
         JOIN holder ON holder.oid = a.grantee
         WHERE att.attnum > 0 AND NOT att.attisdropped
           AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND c.relowner NOT IN (SELECT oid FROM held)
         ORDER BY 1, 2, 5 NULLS FIRST, 3",
        held = held(name, per_membership)
    )
}

pub(super) fn assemble_role_grants(
    result: &QueryResult,
    schemas: &QueryResult,
    owns: &QueryResult,
) -> Result<RoleGrants, DbError> {
    let (database, beyond) = assemble_beyond(owns)?;
    let mut grants = RoleGrants {
        database,
        schemas: Vec::new(),
        held: Vec::new(),
        beyond,
    };
    for row in &schemas.rows {
        let owns = required_cell(schemas, row, "owns")?;
        let rights = assemble_rights(
            schemas,
            row,
            &SCHEMA_PRIVILEGES,
            grants.beyond.superuser,
            &grants.beyond.predefined,
        )?;
        grants.schemas.push(SchemaAccess {
            name: required_cell(schemas, row, "schema")?.to_string(),
            rights,
            owns: owns
                .parse()
                .map_err(|_| super::unexpected_catalog_value("count", owns))?,
        });
    }
    for row in &result.rows {
        grants.held.push(HeldGrant {
            schema: required_cell(result, row, "schema")?.to_string(),
            relation: required_cell(result, row, "relation")?.to_string(),
            privilege: required_cell(result, row, "privilege")?.to_string(),
            grantable: yes(result, row, "grantable")?,
            through: optional_cell(result, row, "through").map(str::to_string),
            usable: yes(result, row, "usable")?,
            sequence: yes(result, row, "sequence")?,
        });
    }
    Ok(grants)
}

pub(super) fn assemble_role_access(access: &QueryResult) -> Result<RoleAccess, DbError> {
    let mut assembled = RoleAccess::default();
    for row in &access.rows {
        assembled.superuser = yes(access, row, "superuser")?;
        assembled.databases.push(DatabaseAccess {
            name: required_cell(access, row, "database")?.to_string(),
            allows_connections: yes(access, row, "allows")?,
            rights: assemble_rights(access, row, &DATABASE_PRIVILEGES, assembled.superuser, &[])?,
        });
    }
    Ok(assembled)
}

/// The rights [`rights_sql`] read from `row`: each privilege that is
/// effective, with the sources that explain it. A superuser holds them all
/// and says only so. One no grant explains is the database owner's or a
/// predefined role's, `predefined` naming the ones the role holds.
fn assemble_rights(
    result: &QueryResult,
    row: &super::Row,
    privileges: &[&str],
    superuser: bool,
    predefined: &[String],
) -> Result<Vec<Right>, DbError> {
    let mut rights = Vec::new();
    for privilege in privileges {
        let column = privilege.to_lowercase();
        if !yes(result, row, &column)? {
            continue;
        }
        let mut sources = Vec::new();
        if superuser {
            sources.push(Source::Superuser);
        } else {
            if let Some(owner) = optional_cell(result, row, "owner") {
                sources.push(Source::Owner(match source_token(owner)? {
                    Source::Via(role) => Some(role),
                    _ => None,
                }));
            }
            let via = optional_cell(result, row, &format!("{column}_via")).unwrap_or_default();
            for token in via.split(SEPARATOR).filter(|token| !token.is_empty()) {
                sources.push(source_token(token)?);
            }
            if sources.is_empty() {
                sources.push(match predefined.is_empty() {
                    true => Source::Implicit,
                    false => Source::Predefined(predefined.join(", ")),
                });
            }
            sources.sort_by_key(Source::rank);
        }
        rights.push(Right {
            privilege: privilege.to_string(),
            sources,
        });
    }
    Ok(rights)
}

/// A [`SOURCE_TOKEN`], read back.
fn source_token(token: &str) -> Result<Source, DbError> {
    match token {
        "direct" => Ok(Source::Direct),
        "public" => Ok(Source::Public),
        _ => token
            .strip_prefix("via:")
            .map(|role| Source::Via(role.to_string()))
            .ok_or_else(|| super::unexpected_catalog_value("grant source", token)),
    }
}

pub(super) fn owns_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT (SELECT pg_catalog.count(*)
                 FROM pg_catalog.pg_class AS c
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema')
                   AND c.relowner IN (SELECT oid FROM held))::text AS owns,
             (SELECT CASE WHEN pg_catalog.bool_or(r.rolsuper) THEN 'y' ELSE 'n' END
              FROM pg_catalog.pg_roles AS r WHERE r.rolname = {}) AS superuser,
             (SELECT pg_catalog.string_agg(r.rolname::text, E'\\x1f' ORDER BY r.rolname)
              FROM held JOIN pg_catalog.pg_roles AS r ON r.oid = held.oid
              WHERE r.rolname ~ '^pg_') AS predefined,
             (SELECT pg_catalog.count(*)
              FROM pg_catalog.pg_default_acl AS d
              WHERE d.defaclrole IN (SELECT oid FROM held)
                 OR EXISTS (SELECT 1
                            FROM pg_catalog.aclexplode(d.defaclacl) AS a
                            JOIN holder ON holder.oid = a.grantee))::text AS defaults,
             pg_catalog.current_database() AS database",
        held(name, per_membership),
        literal(name)
    )
}

pub(super) const PUBLIC_GRANTS_SQL: &str = "
    SELECT DISTINCT n.nspname AS schema, c.relname AS name
    FROM pg_catalog.pg_class AS c
    JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
    CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
    WHERE a.grantee = 0
      AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND n.nspname NOT IN ('pg_catalog', 'information_schema')
    ORDER BY 1, 2";

/// Roles that cannot log in, have no members, own nothing in this database
/// (nor any database), and hold no grant on its schemas, relations or
/// routines. `pg_shdepend` adds what those leave out, in every database of
/// the server: a policy naming it, a column grant, a default privilege, a
/// grant on a database, and anything another database holds of it.
pub(super) const UNUSED_ROLES_SQL: &str = "
    SELECT r.rolname AS name
    FROM pg_catalog.pg_roles AS r
    WHERE NOT r.rolcanlogin
      AND r.rolname !~ '^pg_'
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members AS m WHERE m.roleid = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class AS c
          CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
          WHERE a.grantee = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace AS n
          CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) AS a
          WHERE a.grantee = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_proc AS p
          CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) AS a
          WHERE a.grantee = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class AS c WHERE c.relowner = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace AS n WHERE n.nspowner = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_proc AS p WHERE p.proowner = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_database AS d WHERE d.datdba = r.oid)
      AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_shdepend AS d
          WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass
            AND d.refobjid = r.oid)
    ORDER BY 1";

pub(super) const SESSIONS_SQL: &str = "
    SELECT usename AS name, pg_catalog.count(*)::text AS sessions
    FROM pg_catalog.pg_stat_activity
    WHERE backend_type = 'client backend' AND usename IS NOT NULL
    GROUP BY 1
    ORDER BY pg_catalog.count(*) DESC, 1";

fn yes(result: &QueryResult, row: &super::Row, column: &str) -> Result<bool, DbError> {
    match required_cell(result, row, column)? {
        "y" => Ok(true),
        "n" => Ok(false),
        other => Err(super::unexpected_catalog_value(column, other)),
    }
}

pub(super) fn assemble_roles(
    roles: &QueryResult,
    memberships: &QueryResult,
) -> Result<Roles, DbError> {
    let mut assembled = Roles::default();
    for row in &roles.rows {
        let connection_limit = match optional_cell(roles, row, "connection_limit") {
            Some(limit) => Some(
                limit
                    .parse()
                    .map_err(|_| super::unexpected_catalog_value("connection limit", limit))?,
            ),
            None => None,
        };
        let has_password = match optional_cell(roles, row, "has_password") {
            Some(_) => Some(yes(roles, row, "has_password")?),
            None => None,
        };
        assembled.roles.push(Role {
            name: required_cell(roles, row, "name")?.to_string(),
            login: yes(roles, row, "login")?,
            superuser: yes(roles, row, "superuser")?,
            password_expires: optional_cell(roles, row, "expires").map(str::to_string),
            expired: yes(roles, row, "expired")?,
            connection_limit,
            has_password,
            last_login: None,
        });
    }
    for row in &memberships.rows {
        let member = required_cell(memberships, row, "member")?.to_string();
        let role = required_cell(memberships, row, "role")?.to_string();
        if !yes(memberships, row, "inherits")? {
            assembled.not_inherited.push((member.clone(), role.clone()));
        }
        assembled.memberships.push((member, role));
    }
    Ok(assembled)
}

pub(super) fn assemble_grants(result: &QueryResult, roles: Roles) -> Result<ObjectGrants, DbError> {
    let first = result
        .rows
        .first()
        .ok_or_else(|| plain_error("The server listed no such object.".into()))?;
    let owner = required_cell(result, first, "owner")?.to_string();
    let kind = required_cell(result, first, "kind")?;
    let mut privileges: Vec<String> = privileges_of(kind).iter().map(|p| p.to_string()).collect();
    let mut grants = Vec::new();
    for row in &result.rows {
        let Some(privilege) = optional_cell(result, row, "privilege") else {
            continue;
        };
        if !privileges.iter().any(|known| known == privilege) {
            privileges.push(privilege.to_string());
        }
        grants.push(Grant {
            grantee: optional_cell(result, row, "grantee").map(str::to_string),
            privilege: privilege.to_string(),
            grantable: yes(result, row, "grantable")?,
        });
    }
    Ok(ObjectGrants {
        owner,
        privileges,
        grants,
        roles,
        sequence: kind == "S",
    })
}

pub(super) fn assemble_reach(reach: &QueryResult, owns: &QueryResult) -> Result<Reach, DbError> {
    let mut entries = Vec::new();
    for row in &reach.rows {
        let relations = required_cell(reach, row, "relations")?;
        entries.push(ReachEntry {
            schema: required_cell(reach, row, "schema")?.to_string(),
            privilege: required_cell(reach, row, "privilege")?.to_string(),
            relations: relations
                .parse()
                .map_err(|_| super::unexpected_catalog_value("count", relations))?,
            through: optional_cell(reach, row, "through")
                .map(|through| through.split(SEPARATOR).map(str::to_string).collect())
                .unwrap_or_default(),
        });
    }
    let (database, beyond) = assemble_beyond(owns)?;
    Ok(Reach {
        database,
        entries,
        owns: beyond.owns,
        predefined: beyond.predefined,
    })
}

/// What [`owns_sql`] read: the database it was read in, and the rest.
fn assemble_beyond(owns: &QueryResult) -> Result<(String, Beyond), DbError> {
    let row = owns
        .rows
        .first()
        .ok_or_else(|| plain_error("Catalog query returned no owner count.".into()))?;
    let count = required_cell(owns, row, "owns")?;
    Ok((
        required_cell(owns, row, "database")?.to_string(),
        Beyond {
            superuser: yes(owns, row, "superuser")?,
            owns: count
                .parse()
                .map_err(|_| super::unexpected_catalog_value("count", count))?,
            predefined: optional_cell(owns, row, "predefined")
                .map(|names| names.split(SEPARATOR).map(str::to_string).collect())
                .unwrap_or_default(),
            defaults: {
                let count = required_cell(owns, row, "defaults")?;
                count
                    .parse()
                    .map_err(|_| super::unexpected_catalog_value("count", count))?
            },
        },
    ))
}

pub(super) fn names(result: &QueryResult, column: &str) -> Result<Vec<String>, DbError> {
    (result.rows.iter())
        .map(|row| required_cell(result, row, column).map(str::to_string))
        .collect()
}

pub(super) fn assemble_reports(
    roles: &Roles,
    public: &QueryResult,
    unused: &QueryResult,
    sessions: &QueryResult,
) -> Result<Reports, DbError> {
    let mut public_grants = Vec::new();
    for row in &public.rows {
        let schema = required_cell(public, row, "schema")?;
        let name = required_cell(public, row, "name")?;
        public_grants.push(if schema == "public" {
            name.to_string()
        } else {
            format!("{schema}.{name}")
        });
    }
    let mut by_login = Vec::new();
    for row in &sessions.rows {
        let count = required_cell(sessions, row, "sessions")?;
        by_login.push((
            required_cell(sessions, row, "name")?.to_string(),
            count
                .parse()
                .map_err(|_| super::unexpected_catalog_value("count", count))?,
        ));
    }
    let logins = || roles.roles.iter().filter(|role| role.login);
    Ok(Reports {
        superusers: (roles.roles.iter())
            .filter(|role| role.superuser)
            .map(|role| role.name.clone())
            .collect(),
        idle: None,
        expired: logins()
            .filter(|role| role.expired)
            .map(|role| role.name.clone())
            .collect(),
        public_grants,
        // A login turned off is a login, not a role nobody uses.
        unused_roles: names(unused, "name")?
            .into_iter()
            .filter(|name| !roles.role(name).is_some_and(Role::deactivated))
            .collect(),
        sessions: by_login,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_role(name: &str, superuser: bool) -> Role {
        Role {
            name: name.into(),
            login: true,
            superuser,
            password_expires: None,
            expired: false,
            connection_limit: None,
            has_password: None,
            last_login: None,
        }
    }

    #[test]
    fn dropping_a_role_hands_what_it_owns_to_the_connected_role_first() {
        let text = drop_role_text(&plain_role("ana", false), Some("Boss"), &[]);
        assert_eq!(
            text,
            "-- REASSIGN OWNED and DROP OWNED act on the current database only: run them in every database where \"ana\" owns objects or holds privileges.\n\
             -- Check that \"Boss\" is the role that should take ownership of what \"ana\" owns.\n\
             REASSIGN OWNED BY \"ana\" TO \"Boss\";\n\
             DROP OWNED BY \"ana\";\n\
             DROP ROLE \"ana\";"
        );
    }

    #[test]
    fn dropping_a_role_names_the_listed_databases_and_marks_a_missing_heir() {
        let databases = ["app".to_string(), "audit".to_string()];
        let text = drop_role_text(&plain_role("ana", false), None, &databases);
        assert!(
            text.contains("or holds privileges: app, audit.\n"),
            "{text}"
        );
        assert!(
            text.contains("-- Replace <new_owner> with the role"),
            "{text}"
        );
        assert!(text.contains("TO <new_owner>;"), "{text}");
    }

    #[test]
    fn dropping_the_connected_role_or_a_superuser_still_copies_with_a_warning() {
        let itself = drop_role_text(&plain_role("ana", false), Some("ana"), &[]);
        assert!(
            itself.contains("-- Warning: \"ana\" is the role this connection"),
            "{itself}"
        );
        assert!(
            itself.contains("TO <new_owner>;"),
            "it cannot take over its own: {itself}"
        );
        assert!(itself.ends_with("DROP ROLE \"ana\";"));
        let root = drop_role_text(&plain_role("root", true), Some("ana"), &[]);
        assert!(
            root.contains("-- Warning: \"root\" is a superuser"),
            "{root}"
        );
        assert!(root.contains("TO \"ana\";"));
    }

    #[test]
    fn a_role_name_cannot_end_the_comment_or_the_statement_it_sits_in() {
        let odd = "a\"b\n; DROP ROLE x";
        let text = drop_role_text(&plain_role(odd, false), Some("o\nk"), &["d\nb".to_string()]);
        for line in text.lines().filter(|line| line.starts_with("--")) {
            assert!(!line.contains('\n'));
        }
        // Every line is a comment or one of the three statements: the name's
        // line break stays inside its quotes, in the statements only.
        let comments = text
            .lines()
            .take_while(|line| line.starts_with("--"))
            .count();
        assert_eq!(comments, 2, "{text}");
        assert!(text.contains("\"a\"\"b\n; DROP ROLE x\""), "{text}");
    }

    #[test]
    fn a_new_login_carries_the_verifier_and_never_the_password() {
        let sql = create_role(&NewRole {
            name: "Ana".into(),
            login: true,
            password: Some("hunter2".into()),
            connection_limit: Some(5),
            password_expires: Some("2027-01-31".into()),
            member_of: vec!["reporting".into()],
        });
        assert!(sql.starts_with("CREATE ROLE \"Ana\" LOGIN PASSWORD 'SCRAM-SHA-256$4096:"));
        assert!(!sql.contains("hunter2"));
        assert!(
            sql.ends_with(" CONNECTION LIMIT 5 VALID UNTIL '2027-01-31' IN ROLE \"reporting\";")
        );
    }

    #[test]
    fn a_new_roles_debug_never_shows_its_password() {
        let role = NewRole {
            name: "ana".into(),
            password: Some("hunter2".into()),
            ..NewRole::default()
        };
        let shown = format!("{role:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>") && shown.contains("ana"));
    }

    #[test]
    fn an_edit_writes_only_what_changed() {
        let before = Role {
            name: "ana".into(),
            login: true,
            superuser: false,
            password_expires: Some("2027-01-31".into()),
            expired: false,
            connection_limit: Some(5),
            has_password: None,
            last_login: None,
        };
        let unchanged = RoleEdit {
            name: "ana".into(),
            connection_limit: Some(5),
            password_expires: Some("2027-01-31".into()),
        };
        assert_eq!(edit_role(&before, &unchanged), None);
        let edit = RoleEdit {
            connection_limit: Some(2),
            password_expires: None,
            ..unchanged
        };
        assert_eq!(
            edit_role(&before, &edit).as_deref(),
            Some("ALTER ROLE \"ana\" WITH CONNECTION LIMIT 2 VALID UNTIL 'infinity';")
        );
        let cleared = RoleEdit {
            connection_limit: None,
            ..edit
        };
        assert!(clears_limit(&before, &cleared));
    }

    #[test]
    fn a_role_name_cannot_end_its_literal_whatever_standard_conforming_strings_is() {
        let name = r"x\' OR public.pwn() --";
        let sql = reach_sql(name, true);
        assert!(
            sql.contains(r"rolname = E'x\\'' OR public.pwn() --'"),
            "{sql}"
        );
        assert!(role_grants_sql(name, false).contains(r"E'x\\''"));
        assert!(owns_sql("plain", true).contains("rolname = 'plain'"));
    }

    fn role(name: &str, superuser: bool) -> Role {
        Role {
            name: name.into(),
            login: false,
            superuser,
            password_expires: None,
            expired: false,
            connection_limit: None,
            has_password: None,
            last_login: None,
        }
    }

    #[test]
    fn a_new_role_is_never_made_a_member_of_a_way_into_superuser() {
        let roles = Roles {
            roles: vec![
                role("root", true),
                role("admins", false),
                role("ops", false),
                role("reporting", false),
            ],
            memberships: vec![
                ("admins".into(), "root".into()),
                ("ops".into(), "admins".into()),
                ("reporting".into(), "pg_read_all_data".into()),
            ],
            // Not inheriting still lets a member `SET ROLE`.
            not_inherited: vec![("admins".into(), "root".into())],
            records_logins: false,
        };
        assert!(
            roles
                .refuses_membership("root")
                .unwrap()
                .contains("a superuser")
        );
        let far = roles.refuses_membership("ops").expect("ops reaches root");
        assert!(far.contains("belongs to root"), "{far}");
        for name in [
            "postgres",
            "pg_execute_server_program",
            "pg_write_server_files",
        ] {
            assert!(roles.refuses_membership(name).is_some(), "{name}");
        }
        assert_eq!(roles.refuses_membership("reporting"), None);
        assert_eq!(roles.refuses_membership("pg_read_all_data"), None);
    }

    #[test]
    fn a_role_name_is_quoted_wherever_it_is_written() {
        assert_eq!(
            set_login("o'neil x", false),
            "ALTER ROLE \"o'neil x\" WITH NOLOGIN;"
        );
        assert!(
            reset_password("a\"b", "pw").starts_with("ALTER ROLE \"a\"\"b\" WITH PASSWORD 'SCRAM")
        );
    }

    fn table(columns: &[&str], rows: &[&[Option<&str>]]) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|name| super::super::Column {
                    name: name.to_string(),
                    data_type: None,
                })
                .collect(),
            rows: rows
                .iter()
                .map(|cells| super::super::Row::new(cells.iter().copied()).unwrap())
                .collect(),
            ..QueryResult::default()
        }
    }

    const ACCESS_COLUMNS: [&str; 10] = [
        "database",
        "allows",
        "superuser",
        "connect",
        "connect_via",
        "create",
        "create_via",
        "temporary",
        "temporary_via",
        "owner",
    ];

    #[test]
    fn the_server_wide_read_asks_every_database_with_ownership_and_public_in_it() {
        let sql = database_access_sql("o'neil", true);
        assert!(sql.contains("rolname = 'o''neil'"), "{sql}");
        assert!(
            sql.contains("has_database_privilege('o''neil', d.oid, 'CONNECT')"),
            "{sql}"
        );
        // A database with no list of its own is read as the default one,
        // which gives PUBLIC its CONNECT and TEMPORARY.
        assert!(
            sql.contains("COALESCE(d.datacl, pg_catalog.acldefault('d', d.datdba))"),
            "{sql}"
        );
        assert!(sql.contains("UNION ALL SELECT 0, 'PUBLIC'"), "{sql}");
        assert!(sql.contains("m.inherit_option"), "{sql}");
        assert!(database_access_sql("x", false).contains("member.rolinherit"));
        // One read, not one per database, and nothing that writes.
        assert!(!sql.to_uppercase().contains("INSERT"), "{sql}");
        assert_eq!(sql.matches("pg_database").count(), 1, "{sql}");
    }

    #[test]
    fn the_per_database_reads_are_scoped_to_the_schema_and_say_whether_it_is_usable() {
        let schemas = schema_access_sql("ana", true);
        assert!(schemas.contains("has_schema_privilege('ana', n.oid, 'USAGE')"));
        assert!(schemas.contains("COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner))"));
        assert!(!schemas.contains("pg_database"), "{schemas}");
        let relations = role_grants_sql("ana", true);
        assert!(relations.contains("has_schema_privilege('ana', n.oid, 'USAGE')"));
        assert!(
            relations.contains("pg_catalog.aclexplode(att.attacl)"),
            "columns"
        );
        assert!(
            relations.contains("JOIN holder ON holder.oid = a.grantee"),
            "PUBLIC"
        );
        assert!(owns_sql("ana", true).contains("pg_catalog.pg_default_acl"));
    }

    #[test]
    fn a_database_a_role_reaches_by_public_a_role_and_ownership_says_which() {
        let access = table(
            &ACCESS_COLUMNS,
            &[
                // PUBLIC's default CONNECT and TEMPORARY, and a role's CREATE.
                &[
                    Some("app"),
                    Some("y"),
                    Some("n"),
                    Some("y"),
                    Some("public"),
                    Some("y"),
                    Some("via:builders"),
                    Some("y"),
                    Some("public"),
                    None,
                ],
                // Owned through a role.
                &[
                    Some("mine"),
                    Some("y"),
                    Some("n"),
                    Some("y"),
                    None,
                    Some("y"),
                    None,
                    Some("y"),
                    None,
                    Some("via:owners"),
                ],
                // Connect revoked from PUBLIC, granted to the role itself
                // and, as well, through a role.
                &[
                    Some("private"),
                    Some("y"),
                    Some("n"),
                    Some("y"),
                    Some("direct\u{1f}via:team"),
                    Some("n"),
                    None,
                    Some("n"),
                    None,
                    None,
                ],
                &[
                    Some("closed"),
                    Some("n"),
                    Some("n"),
                    Some("n"),
                    None,
                    Some("n"),
                    None,
                    Some("n"),
                    None,
                    None,
                ],
            ],
        );
        let access = assemble_role_access(&access).unwrap();
        let by_name = |name: &str| access.databases.iter().find(|d| d.name == name).unwrap();
        let sources = |name: &str, privilege: &str| {
            (by_name(name).rights.iter())
                .find(|right| right.privilege == privilege)
                .map(|right| right.sources.clone())
        };
        assert_eq!(sources("app", "CONNECT"), Some(vec![Source::Public]));
        assert_eq!(
            sources("app", "CREATE"),
            Some(vec![Source::Via("builders".into())])
        );
        assert_eq!(
            sources("mine", "CONNECT"),
            Some(vec![Source::Owner(Some("owners".into()))])
        );
        assert_eq!(
            sources("private", "CONNECT"),
            Some(vec![Source::Direct, Source::Via("team".into())])
        );
        assert_eq!(sources("private", "TEMPORARY"), None);
        assert!(by_name("private").connects());
        assert!(!by_name("closed").connects());
        // Connect granted, but the database takes no connections.
        let mut shut = by_name("app").clone();
        shut.allows_connections = false;
        assert!(!shut.connects());
    }

    #[test]
    fn a_superuser_holds_every_privilege_and_says_only_that() {
        let access = assemble_role_access(&table(
            &ACCESS_COLUMNS,
            &[&[
                Some("app"),
                Some("y"),
                Some("y"),
                Some("y"),
                Some("public"),
                Some("y"),
                None,
                Some("y"),
                None,
                None,
            ]],
        ))
        .unwrap();
        assert!(access.superuser);
        assert!(
            access.databases[0]
                .rights
                .iter()
                .all(|right| right.sources == [Source::Superuser])
        );
    }

    #[test]
    fn a_privilege_no_grant_explains_is_the_database_owners_or_a_predefined_roles() {
        let schemas = table(
            &[
                "schema",
                "usage",
                "usage_via",
                "create",
                "create_via",
                "owner",
                "owns",
            ],
            &[
                &[
                    Some("public"),
                    Some("y"),
                    Some("public"),
                    Some("y"),
                    None,
                    None,
                    Some("0"),
                ],
                &[
                    Some("data"),
                    Some("y"),
                    None,
                    Some("n"),
                    None,
                    None,
                    Some("2"),
                ],
            ],
        );
        let owns = |predefined: Option<&str>| {
            table(
                &["owns", "superuser", "predefined", "defaults", "database"],
                &[&[Some("2"), Some("n"), predefined, Some("1"), Some("shop")]],
            )
        };
        let none = table(
            &[
                "schema",
                "relation",
                "privilege",
                "grantable",
                "through",
                "database",
                "usable",
                "sequence",
            ],
            &[],
        );
        let plain = assemble_role_grants(&none, &schemas, &owns(None)).unwrap();
        assert_eq!(plain.database, "shop");
        assert_eq!(plain.beyond.defaults, 1);
        assert_eq!(
            plain.schemas[0].rights[1].sources,
            [Source::Implicit],
            "CREATE on public held by the database owner, nothing says so"
        );
        assert_eq!(plain.schemas[1].owns, 2);
        let read = assemble_role_grants(&none, &schemas, &owns(Some("pg_read_all_data"))).unwrap();
        assert_eq!(
            read.schemas[1].rights[0].sources,
            [Source::Predefined("pg_read_all_data".into())]
        );
        assert!(read.schemas[1].usable());
    }

    #[test]
    fn a_relation_grant_keeps_where_it_came_from_and_whether_its_schema_opens() {
        let held = table(
            &[
                "schema",
                "relation",
                "privilege",
                "grantable",
                "through",
                "database",
                "usable",
                "sequence",
            ],
            &[
                &[
                    Some("sales"),
                    Some("orders"),
                    Some("SELECT"),
                    Some("n"),
                    Some("PUBLIC"),
                    Some("shop"),
                    Some("y"),
                    Some("n"),
                ],
                &[
                    Some("sales"),
                    Some("orders_id_seq"),
                    Some("USAGE"),
                    Some("y"),
                    None,
                    Some("shop"),
                    Some("n"),
                    Some("y"),
                ],
            ],
        );
        let schemas = table(
            &[
                "schema",
                "usage",
                "usage_via",
                "create",
                "create_via",
                "owner",
                "owns",
            ],
            &[],
        );
        let owns = table(
            &["owns", "superuser", "predefined", "defaults", "database"],
            &[&[Some("0"), Some("n"), None, Some("0"), Some("shop")]],
        );
        let grants = assemble_role_grants(&held, &schemas, &owns).unwrap();
        assert_eq!(grants.held[0].through.as_deref(), Some("PUBLIC"));
        assert!(grants.held[0].usable && !grants.held[1].usable);
        assert!(grants.held[1].grantable && grants.held[1].through.is_none());
        assert!(!grants.held[0].sequence && grants.held[1].sequence);
        let bad = table(
            &[
                "database",
                "allows",
                "superuser",
                "connect",
                "connect_via",
                "create",
                "create_via",
                "temporary",
                "temporary_via",
                "owner",
            ],
            &[&[
                Some("x"),
                Some("y"),
                Some("n"),
                Some("y"),
                Some("sideways"),
                Some("n"),
                None,
                Some("n"),
                None,
                None,
            ]],
        );
        assert!(assemble_role_access(&bad).is_err());
    }

    fn change(
        database: Option<&str>,
        object: GrantObject,
        grantee: &str,
        privilege: &str,
        action: GrantAction,
    ) -> GrantChange {
        GrantChange {
            database: database.map(str::to_string),
            object,
            grantee: grantee.into(),
            privilege: privilege.into(),
            action,
        }
    }

    fn on_table(schema: &str, name: &str) -> GrantObject {
        GrantObject::Relation {
            schema: schema.into(),
            name: name.into(),
            sequence: false,
        }
    }

    #[test]
    fn grants_and_revokes_are_spelled_per_object_in_the_dialogs_order() {
        use GrantAction::{Grant, Revoke};
        let sequence = GrantObject::Relation {
            schema: "s".into(),
            name: "q".into(),
            sequence: true,
        };
        let batches = grant_statements(&[
            change(None, on_table("s", "t"), "r", "INSERT", Grant),
            change(None, on_table("s", "t"), "r", "SELECT", Grant),
            change(None, on_table("s", "t"), "r", "SELECT", Grant),
            change(None, sequence, "r", "USAGE", Revoke),
            change(None, GrantObject::Schema("s".into()), "r", "USAGE", Grant),
            change(
                None,
                GrantObject::Database("d".into()),
                "r",
                "CONNECT",
                Grant,
            ),
        ]);
        assert_eq!(
            batches,
            [GrantBatch {
                database: None,
                sql: "REVOKE USAGE ON SEQUENCE \"s\".\"q\" FROM \"r\";\n\
                      GRANT CONNECT ON DATABASE \"d\" TO \"r\";\n\
                      GRANT USAGE ON SCHEMA \"s\" TO \"r\";\n\
                      GRANT SELECT, INSERT ON TABLE \"s\".\"t\" TO \"r\";"
                    .into()
            }]
        );
    }

    #[test]
    fn a_grant_quotes_every_name() {
        let batches = grant_statements(&[change(
            None,
            on_table("Sa\"les", "Or ders"),
            "An\"a",
            "SELECT",
            GrantAction::Grant,
        )]);
        assert_eq!(
            batches[0].sql,
            "GRANT SELECT ON TABLE \"Sa\"\"les\".\"Or ders\" TO \"An\"\"a\";"
        );
    }

    #[test]
    fn changes_group_by_database_the_connections_own_first() {
        use GrantAction::Grant;
        let batches = grant_statements(&[
            change(Some("zeta"), on_table("s", "t"), "r", "SELECT", Grant),
            change(Some("alpha"), on_table("s", "t"), "r", "SELECT", Grant),
            change(
                None,
                GrantObject::Database("d".into()),
                "r",
                "CONNECT",
                Grant,
            ),
            change(Some("zeta"), on_table("s", "u"), "r", "SELECT", Grant),
        ]);
        let databases: Vec<_> = batches.iter().map(|b| b.database.as_deref()).collect();
        assert_eq!(databases, [None, Some("alpha"), Some("zeta")]);
        assert_eq!(batches[2].sql.lines().count(), 2);
    }

    #[test]
    fn a_privilege_the_object_cannot_take_is_never_written() {
        use GrantAction::Grant;
        let database = GrantObject::Database("d".into());
        assert!(grant_applies(&database, "TEMPORARY"));
        assert!(!grant_applies(&database, "SELECT"));
        assert!(!grant_applies(&on_table("s", "t"), "USAGE"));
        assert!(!grant_applies(&on_table("s", "t"), "ALL"));
        let batches = grant_statements(&[
            change(None, database.clone(), "r", "ALL", Grant),
            change(None, on_table("s", "t"), "r", "USAGE", Grant),
            change(None, database, "r", "SELECT", Grant),
        ]);
        assert!(batches.is_empty());
    }
}
