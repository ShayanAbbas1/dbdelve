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
}

/// Every privilege one role holds on a relation of the connection's
/// database, itself or through a role it belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleGrants {
    pub database: String,
    pub held: Vec<HeldGrant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldGrant {
    pub schema: String,
    pub relation: String,
    /// As the server spells it, `SELECT`.
    pub privilege: String,
    pub grantable: bool,
    /// The role it is held through; `None` for the role's own grant.
    pub through: Option<String>,
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

/// A role to create.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewRole {
    pub name: String,
    pub login: bool,
    pub password: Option<String>,
    pub connection_limit: Option<i64>,
    /// `YYYY-MM-DD`.
    pub password_expires: Option<String>,
    pub member_of: Vec<String>,
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

fn ident(name: &str) -> String {
    Engine::Postgres.quote_identifier(name)
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

/// ponytail: grants on single columns are not counted; `pg_attribute.attacl`
/// is where they are.
pub(super) fn reach_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT n.nspname AS schema, a.privilege_type AS privilege,
             pg_catalog.count(DISTINCT c.oid)::text AS relations,
             pg_catalog.string_agg(DISTINCT holder.via::text, E'\\x1f') AS through
         FROM pg_catalog.pg_class AS c
         JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
         JOIN holder ON holder.oid = a.grantee
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname !~ '^pg_toast'
           AND c.relowner NOT IN (SELECT oid FROM held)
         GROUP BY 1, 2
         ORDER BY 1, 2",
        held(name, per_membership)
    )
}

/// Owned relations are left out: an owner holds everything on its own.
pub(super) fn role_grants_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT n.nspname AS schema, c.relname AS relation, a.privilege_type AS privilege,
             CASE WHEN a.is_grantable THEN 'y' ELSE 'n' END AS grantable,
             holder.via AS through, pg_catalog.current_database() AS database
         FROM pg_catalog.pg_class AS c
         JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
         CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) AS a
         JOIN holder ON holder.oid = a.grantee
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND c.relowner NOT IN (SELECT oid FROM held)
         ORDER BY 1, 2, holder.via NULLS FIRST",
        held(name, per_membership)
    )
}

pub(super) fn assemble_role_grants(result: &QueryResult) -> Result<RoleGrants, DbError> {
    let mut grants = RoleGrants::default();
    for row in &result.rows {
        grants.database = required_cell(result, row, "database")?.to_string();
        grants.held.push(HeldGrant {
            schema: required_cell(result, row, "schema")?.to_string(),
            relation: required_cell(result, row, "relation")?.to_string(),
            privilege: required_cell(result, row, "privilege")?.to_string(),
            grantable: yes(result, row, "grantable")?,
            through: optional_cell(result, row, "through").map(str::to_string),
        });
    }
    Ok(grants)
}

pub(super) fn owns_sql(name: &str, per_membership: bool) -> String {
    format!(
        "{}
         SELECT (SELECT pg_catalog.count(*)
                 FROM pg_catalog.pg_class AS c
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
                   AND n.nspname NOT IN ('pg_catalog', 'information_schema')
                   AND c.relowner IN (SELECT oid FROM held))::text AS owns,
             (SELECT pg_catalog.string_agg(r.rolname::text, E'\\x1f' ORDER BY r.rolname)
              FROM held JOIN pg_catalog.pg_roles AS r ON r.oid = held.oid
              WHERE r.rolname ~ '^pg_') AS predefined,
             pg_catalog.current_database() AS database",
        held(name, per_membership)
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
/// routines. Another database's grants are not seen from here.
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
    let row = owns
        .rows
        .first()
        .ok_or_else(|| plain_error("Catalog query returned no owner count.".into()))?;
    let count = required_cell(owns, row, "owns")?;
    Ok(Reach {
        database: required_cell(owns, row, "database")?.to_string(),
        entries,
        owns: count
            .parse()
            .map_err(|_| super::unexpected_catalog_value("count", count))?,
        predefined: optional_cell(owns, row, "predefined")
            .map(|names| names.split(SEPARATOR).map(str::to_string).collect())
            .unwrap_or_default(),
    })
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
}
