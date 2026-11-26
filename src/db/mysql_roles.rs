//! MySQL's and MariaDB's accounts, roles and grants, read for the Users and
//! roles tab as the plain data of [`super::roles`]. Read only: nothing here
//! writes, and no statement it sends changes an account.
//!
//! An account is a user at a host, `app@%`, and that pair is its name on
//! screen; a MariaDB role has no host and is shown as its name alone. What an
//! account holds is read from `SHOW GRANTS`, which says every level at once
//! (`*.*`, a database, a table, its columns, a routine), so one read of an
//! account answers for every database. Its text is parsed here, and a line
//! this parser cannot read is kept and shown as unread, never dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::roles::{
    Beyond, DatabaseAccess, Grant, HeldGrant, ObjectGrants, Reach, ReachEntry, Reports, Right,
    Role, RoleAccess, RoleGrants, Roles, Source,
};
use super::{DbError, Engine, QueryResult, Row, optional_cell, plain_error, required_cell};

/// The server's codes for a statement refused for want of a privilege:
/// table, column, database and global. 1045 is a failed login, not this.
pub(super) const DENIED_CODES: [u16; 4] = [1142, 1143, 1044, 1227];

const DENIED: &str =
    "This login has no privilege to list users: reading them needs SELECT on mysql.user.";

const GRANTS_DENIED: &str = "This login has no privilege to list grants: reading them needs \
     SELECT on mysql.user, mysql.db, mysql.tables_priv and mysql.columns_priv.";

/// The privileges a table can be granted, in the order Show Grants lists
/// them, with the column `mysql.user` and `mysql.db` keep each in.
const TABLE_PRIVILEGES: [(&str, &str); 12] = [
    ("SELECT", "Select_priv"),
    ("INSERT", "Insert_priv"),
    ("UPDATE", "Update_priv"),
    ("DELETE", "Delete_priv"),
    ("CREATE", "Create_priv"),
    ("DROP", "Drop_priv"),
    ("REFERENCES", "References_priv"),
    ("INDEX", "Index_priv"),
    ("ALTER", "Alter_priv"),
    ("CREATE VIEW", "Create_view_priv"),
    ("SHOW VIEW", "Show_view_priv"),
    ("TRIGGER", "Trigger_priv"),
];

/// Privileges that act on what is inside a database. Granted on `*.*` they
/// reach every database alike, so they are listed under each; every other
/// global privilege is over the server itself.
const DATA_PRIVILEGES: [&str; 21] = [
    "ALL PRIVILEGES",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "REFERENCES",
    "INDEX",
    "ALTER",
    "CREATE TEMPORARY TABLES",
    "LOCK TABLES",
    "EXECUTE",
    "CREATE VIEW",
    "SHOW VIEW",
    "CREATE ROUTINE",
    "ALTER ROUTINE",
    "EVENT",
    "TRIGGER",
    "DELETE HISTORY",
    "GRANT OPTION",
];

/// The global privileges taken as a superuser's.
///
/// ponytail: a superuser here is one holding these with `GRANT`, which is
/// most of what `ALL PRIVILEGES ... WITH GRANT OPTION` gives; MySQL 8's
/// dynamic `*_ADMIN` privileges and MariaDB 11's split of `SUPER` are not
/// weighed. Reading every privilege column would be the exact test.
const SUPERUSER_PRIVILEGES: [&str; 7] = [
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "SUPER",
];

/// One account: `user@host`, or a MariaDB role, which has no host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Account {
    pub user: String,
    pub host: Option<String>,
}

impl Account {
    /// An account's name as shown, read back: split at the last `@`, since a
    /// user name may hold one and a host never does.
    ///
    /// ponytail: a MariaDB role whose own name holds an `@` reads as a user
    /// at a host. Resolving the name against the listed accounts would tell
    /// them apart.
    pub fn parse(name: &str) -> Self {
        match name.rsplit_once('@') {
            Some((user, host)) => Self {
                user: user.into(),
                host: Some(host.into()),
            },
            None => Self {
                user: name.into(),
                host: None,
            },
        }
    }

    pub fn display(&self) -> String {
        match &self.host {
            Some(host) => format!("{}@{host}", self.user),
            None => self.user.clone(),
        }
    }

    /// As `SHOW GRANTS FOR` takes it, which cannot bind a parameter:
    /// `'user'@'host'`, or `'role'`.
    pub fn quoted(&self) -> String {
        match &self.host {
            Some(host) => format!("{}@{}", quote(&self.user), quote(host)),
            None => quote(&self.user),
        }
    }

    /// As a grant line shows it to a reader, unescaped: `'app'@'%'`.
    fn shown_quoted(&self) -> String {
        match &self.host {
            Some(host) => format!("'{}'@'{host}'", self.user),
            None => format!("'{}'", self.user),
        }
    }

    /// The account of a grant table's `User` and `Host`: MariaDB keeps a
    /// role with an empty host.
    fn of(engine: Engine, user: &str, host: &str) -> Self {
        Self {
            user: user.into(),
            host: (engine != Engine::MariaDb || !host.is_empty()).then(|| host.into()),
        }
    }
}

/// A single-quoted MySQL string: the quote doubled and the backslash, which
/// escapes in MySQL's default mode, escaped.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', r"\\").replace('\'', "''"))
}

/// Where a grant is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Level {
    /// `*.*`.
    Global,
    /// `db.*`, the name as written: a `LIKE` pattern when it holds an
    /// unescaped `_` or `%`.
    Database(String),
    Table {
        database: String,
        table: String,
    },
    Routine {
        database: String,
        name: String,
        /// `procedure` or `function`.
        kind: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Privilege {
    /// Upper case, words joined by one space: `CREATE TEMPORARY TABLES`.
    pub name: String,
    /// The columns it is granted on, none for the whole object.
    pub columns: Vec<String>,
}

/// One line of `SHOW GRANTS`, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Line {
    /// `USAGE`, which grants nothing, is left out of `privileges`.
    Privileges {
        privileges: Vec<Privilege>,
        level: Level,
        grantee: Account,
        grantable: bool,
    },
    Proxy {
        target: Account,
        grantee: Account,
        grantable: bool,
    },
    /// Roles granted to `grantee`.
    Roles {
        roles: Vec<Account>,
        grantee: Account,
    },
    /// A partial revoke, which MySQL lists with `partial_revokes` on: a
    /// global privilege taken off one database, `REVOKE SELECT ON mysql.*`.
    Revoke {
        privileges: Vec<Privilege>,
        database: String,
        grantee: Account,
    },
    /// What says nothing of what is held: MariaDB's `SET DEFAULT ROLE`.
    Skipped,
    /// A line this parser cannot read, cut short and with anything after
    /// `IDENTIFIED` or `PASSWORD` left out, so no hash is ever kept.
    Unread(String),
}

impl Line {
    fn grantee(&self) -> Option<&Account> {
        match self {
            Self::Privileges { grantee, .. }
            | Self::Proxy { grantee, .. }
            | Self::Roles { grantee, .. }
            | Self::Revoke { grantee, .. } => Some(grantee),
            Self::Skipped | Self::Unread(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    /// A backticked name.
    Name(String),
    /// A quoted string.
    Text(String),
    Punct(char),
}

/// The tokens of a grant line; `None` for a quote left open.
fn tokens(line: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '`' => {
                let mut name = String::new();
                loop {
                    match chars.next()? {
                        '`' if chars.peek() == Some(&'`') => {
                            chars.next();
                            name.push('`');
                        }
                        '`' => break,
                        c => name.push(c),
                    }
                }
                tokens.push(Token::Name(name));
            }
            '\'' | '"' => {
                let mut text = String::new();
                loop {
                    match chars.next()? {
                        // `\_` and `\%` keep their backslash: MySQL leaves
                        // it for `LIKE` to read.
                        '\\' => match chars.next()? {
                            escaped @ ('_' | '%') => {
                                text.push('\\');
                                text.push(escaped);
                            }
                            'n' => text.push('\n'),
                            't' => text.push('\t'),
                            '0' => text.push('\0'),
                            escaped => text.push(escaped),
                        },
                        q if q == c && chars.peek() == Some(&c) => {
                            chars.next();
                            text.push(c);
                        }
                        q if q == c => break,
                        other => text.push(other),
                    }
                }
                tokens.push(Token::Text(text));
            }
            ',' | '.' | '(' | ')' | '@' | ';' => tokens.push(Token::Punct(c)),
            c => {
                let mut word = String::from(c);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || "`'\",.()@;".contains(next) {
                        break;
                    }
                    word.push(next);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Some(tokens)
}

fn is_word(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
}

/// A name as written: backticked, quoted or bare.
fn name(token: Option<&Token>) -> Option<String> {
    match token? {
        Token::Name(name) | Token::Text(name) | Token::Word(name) => Some(name.clone()),
        Token::Punct(_) => None,
    }
}

/// The account at the start of `tokens`, and how many tokens it took.
/// `SHOW GRANTS` always quotes an account's parts, so a bare word is not
/// one.
fn account(tokens: &[Token]) -> Option<(Account, usize)> {
    let quoted = |token: Option<&Token>| match token? {
        Token::Name(name) | Token::Text(name) => Some(name.clone()),
        Token::Word(_) | Token::Punct(_) => None,
    };
    let user = quoted(tokens.first())?;
    if tokens.get(1) != Some(&Token::Punct('@')) {
        return Some((Account { user, host: None }, 1));
    }
    let host = quoted(tokens.get(2))?;
    Some((
        Account {
            user,
            host: Some(host),
        },
        3,
    ))
}

/// Whether the words `first second` follow one another in `tokens`.
fn has_words(tokens: &[Token], first: &str, second: &str) -> bool {
    (tokens.windows(2)).any(|pair| is_word(pair.first(), first) && is_word(pair.get(1), second))
}

/// A comma-separated privilege list, `SELECT, UPDATE (a, b), LOCK TABLES`,
/// `USAGE` left out.
fn privileges(tokens: &[Token]) -> Option<Vec<Privilege>> {
    let mut privileges = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut columns = Vec::new();
    let mut index = 0;
    let mut finish = |words: &mut Vec<String>, columns: &mut Vec<String>| -> Option<()> {
        if words.is_empty() {
            return None;
        }
        let name = match words.join(" ").to_uppercase() {
            all if all == "ALL" => "ALL PRIVILEGES".to_string(),
            name => name,
        };
        if name != "USAGE" {
            privileges.push(Privilege {
                name,
                columns: std::mem::take(columns),
            });
        }
        words.clear();
        columns.clear();
        Some(())
    };
    while let Some(token) = tokens.get(index) {
        match token {
            Token::Word(word) if columns.is_empty() => words.push(word.clone()),
            Token::Punct('(') => {
                index += 1;
                loop {
                    columns.push(name(tokens.get(index))?);
                    index += 1;
                    match tokens.get(index)? {
                        Token::Punct(',') => index += 1,
                        Token::Punct(')') => break,
                        _ => return None,
                    }
                }
            }
            Token::Punct(',') => finish(&mut words, &mut columns)?,
            _ => return None,
        }
        index += 1;
    }
    finish(&mut words, &mut columns)?;
    Some(privileges)
}

/// What follows `ON`: `*.*`, `db.*`, `db.tbl`, `PROCEDURE db.p`.
fn level(tokens: &[Token]) -> Option<Level> {
    let (kind, rest) = match tokens.first() {
        Some(token) if is_word(Some(token), "PROCEDURE") => (Some("procedure"), &tokens[1..]),
        Some(token) if is_word(Some(token), "FUNCTION") => (Some("function"), &tokens[1..]),
        Some(token) if is_word(Some(token), "TABLE") => (None, &tokens[1..]),
        _ => (None, tokens),
    };
    let [first, Token::Punct('.'), second] = rest else {
        return None;
    };
    let wildcard = |token: &Token| *token == Token::Word("*".into());
    match (wildcard(first), wildcard(second), kind) {
        (true, true, None) => Some(Level::Global),
        (false, true, None) => Some(Level::Database(name(Some(first))?)),
        (false, false, None) => Some(Level::Table {
            database: name(Some(first))?,
            table: name(Some(second))?,
        }),
        (false, false, Some(kind)) => Some(Level::Routine {
            database: name(Some(first))?,
            name: name(Some(second))?,
            kind,
        }),
        _ => None,
    }
}

/// One line of `SHOW GRANTS` output. Never fails: what it cannot read is
/// [`Line::Unread`].
pub(super) fn parse_grant(line: &str) -> Line {
    read_grant(line).unwrap_or_else(|| unread(line))
}

fn read_grant(line: &str) -> Option<Line> {
    let mut tokens = tokens(line)?;
    if tokens.last() == Some(&Token::Punct(';')) {
        tokens.pop();
    }
    if is_word(tokens.first(), "SET")
        && is_word(tokens.get(1), "DEFAULT")
        && is_word(tokens.get(2), "ROLE")
    {
        return Some(Line::Skipped);
    }
    if is_word(tokens.first(), "REVOKE") {
        return read_revoke(&tokens);
    }
    if !is_word(tokens.first(), "GRANT") {
        return None;
    }
    let to = tokens.iter().position(|token| is_word(Some(token), "TO"))?;
    let (grantee, taken) = account(&tokens[to + 1..])?;
    let tail = &tokens[to + 1 + taken..];
    let grantable = has_words(tail, "GRANT", "OPTION");
    let on = tokens[..to]
        .iter()
        .position(|token| is_word(Some(token), "ON"));
    if is_word(tokens.get(1), "PROXY") && on == Some(2) {
        let (target, taken) = account(&tokens[3..to])?;
        return (3 + taken == to).then_some(Line::Proxy {
            target,
            grantee,
            grantable,
        });
    }
    match on {
        Some(on) => Some(Line::Privileges {
            privileges: privileges(&tokens[1..on])?,
            level: level(&tokens[on + 1..to])?,
            grantee,
            grantable,
        }),
        None => {
            let mut roles = Vec::new();
            for part in tokens[1..to].split(|token| *token == Token::Punct(',')) {
                let (role, taken) = account(part)?;
                if taken != part.len() {
                    return None;
                }
                roles.push(role);
            }
            Some(Line::Roles { roles, grantee })
        }
    }
}

/// `REVOKE privileges ON db.* FROM account`, the one form `SHOW GRANTS`
/// lists a revoke in.
fn read_revoke(tokens: &[Token]) -> Option<Line> {
    let from = tokens
        .iter()
        .position(|token| is_word(Some(token), "FROM"))?;
    let on = tokens[..from]
        .iter()
        .position(|token| is_word(Some(token), "ON"))?;
    let (grantee, taken) = account(&tokens[from + 1..])?;
    if from + 1 + taken != tokens.len() {
        return None;
    }
    let Level::Database(database) = level(&tokens[on + 1..from])? else {
        return None;
    };
    Some(Line::Revoke {
        privileges: privileges(&tokens[1..on])?,
        database,
        grantee,
    })
}

const UNREAD_CHARS: usize = 60;

fn unread(line: &str) -> Line {
    let upper = line.to_ascii_uppercase();
    let cut = ["IDENTIFIED", "PASSWORD"]
        .iter()
        .filter_map(|word| upper.find(word))
        .min()
        .unwrap_or(line.len());
    Line::Unread(line[..cut].trim_end().chars().take(UNREAD_CHARS).collect())
}

/// Whether a database name in a grant is a `LIKE` pattern: it holds a `_`
/// or `%` no backslash escapes.
pub(super) fn is_pattern(name: &str) -> bool {
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => _ = chars.next(),
            '_' | '%' => return true,
            _ => {}
        }
    }
    false
}

/// A name as `LIKE` reads it with nothing to match: its escapes dropped.
fn unescaped(name: &str) -> String {
    let mut out = String::new();
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

/// `name` as a pattern matching only itself: every `_` and `%` no
/// backslash escapes, escaped. How a schema-level grant reads where the
/// server takes the name literally, as MySQL does with `partial_revokes` on.
fn literal(name: &str) -> String {
    let mut out = String::new();
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                out.extend(chars.next());
            }
            '_' | '%' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// `held` as a server that takes schema names in grants literally reads
/// it: a database-level grant on `sales_1` reaches `sales_1` alone.
fn literal_levels(held: Held) -> Held {
    (held.into_iter())
        .map(|(through, line)| match line {
            Line::Privileges {
                privileges,
                level: Level::Database(name),
                grantee,
                grantable,
            } => (
                through,
                Line::Privileges {
                    privileges,
                    level: Level::Database(literal(&name)),
                    grantee,
                    grantable,
                },
            ),
            line => (through, line),
        })
        .collect()
}

/// Whether `name` matches the `LIKE` pattern `pattern`, `\` escaping.
///
/// ponytail: case-sensitive, as a server with `lower_case_table_names=0`
/// compares database names; one with 1 or 2 would match case-insensitively.
pub(super) fn like_matches(pattern: &str, name: &str) -> bool {
    enum Part {
        Any,
        One,
        Char(char),
    }
    let mut parts = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        parts.push(match c {
            '%' => Part::Any,
            '_' => Part::One,
            '\\' => Part::Char(chars.next().unwrap_or('\\')),
            c => Part::Char(c),
        });
    }
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match parts.get(p) {
            Some(Part::Any) => {
                star = Some((p, n));
                p += 1;
            }
            Some(Part::One) => (p, n) = (p + 1, n + 1),
            Some(Part::Char(c)) if *c == name[n] => (p, n) = (p + 1, n + 1),
            _ => match star {
                Some((at, from)) => {
                    star = Some((at, from + 1));
                    (p, n) = (at + 1, from + 1);
                }
                None => return false,
            },
        }
    }
    parts[p..].iter().all(|part| matches!(part, Part::Any))
}

/// How a database-level grant written as `granted` reaches the database
/// `database`, if it does: by its own name, or as a pattern.
fn database_source(granted: &str, database: &str) -> Option<Source> {
    if granted == database || (!is_pattern(granted) && unescaped(granted) == database) {
        Some(Source::Direct)
    } else if is_pattern(granted) && like_matches(granted, database) {
        Some(Source::Pattern(format!("{granted}.*")))
    } else {
        None
    }
}

/// Whether the server has roles: MySQL 8 and MariaDB 10.0.5 on.
fn has_roles(engine: Engine, version: (u16, u16, u16)) -> bool {
    match engine {
        Engine::MariaDb => version >= (10, 0, 5),
        _ => version >= (8, 0, 0),
    }
}

/// Every account of the server, with what [`Role`] says of it. The column
/// set follows the server: MySQL 5.7.6 brought `account_locked` and
/// password lifetimes, 5.6.6 `password_expired`; MariaDB keeps roles in
/// `is_role` and, from 10.4, an account's lock and lifetime in
/// `mysql.global_priv`, whose JSON `mysql.user` (a view there) leaves out.
/// No password hash leaves the server: only whether there is one.
pub(super) fn accounts_sql(engine: Engine, version: (u16, u16, u16)) -> String {
    let superuser = [
        "Select", "Insert", "Update", "Delete", "Create", "Drop", "Grant", "Super",
    ]
    .map(|privilege| format!("u.{privilege}_priv = 'Y'"))
    .join(" AND ");
    let none = "NULL";
    let no = "'n'";
    let (password, locked, role, expired, expires, default_role, join) = match engine {
        Engine::MariaDb if version >= (10, 4, 0) => {
            let lifetime = "CAST(JSON_VALUE(g.Priv, '$.password_lifetime') AS SIGNED)";
            let ends = format!(
                "FROM_UNIXTIME(CAST(JSON_VALUE(g.Priv, '$.password_last_changed') AS SIGNED) \
                 + {lifetime} * 86400)"
            );
            (
                "u.Password <> '' OR u.authentication_string <> ''".to_string(),
                // JSON_VALUE reads the boolean back as `1`.
                "JSON_VALUE(g.Priv, '$.account_locked') IN ('1', 'true')".to_string(),
                "u.is_role = 'Y'".to_string(),
                format!("u.password_expired = 'Y' OR ({lifetime} > 0 AND {ends} < NOW())"),
                format!("CASE WHEN {lifetime} > 0 THEN DATE_FORMAT({ends}, '%Y-%m-%d') END"),
                "u.default_role".to_string(),
                "LEFT JOIN mysql.global_priv AS g ON g.User = u.User AND g.Host = u.Host",
            )
        }
        Engine::MariaDb => (
            "u.Password <> '' OR u.authentication_string <> ''".to_string(),
            no.into(),
            "u.is_role = 'Y'".to_string(),
            no.into(),
            none.into(),
            "u.default_role".to_string(),
            "",
        ),
        _ if version >= (5, 7, 6) => {
            let ends = "DATE_ADD(u.password_last_changed, INTERVAL u.password_lifetime DAY)";
            (
                "u.authentication_string <> ''".to_string(),
                "u.account_locked = 'Y'".to_string(),
                no.into(),
                format!("u.password_expired = 'Y' OR (u.password_lifetime > 0 AND {ends} < NOW())"),
                format!(
                    "CASE WHEN u.password_lifetime > 0 THEN DATE_FORMAT({ends}, '%Y-%m-%d') END"
                ),
                none.into(),
                "",
            )
        }
        _ => (
            "u.Password <> '' OR u.authentication_string <> ''".to_string(),
            no.into(),
            no.into(),
            match version >= (5, 6, 6) {
                true => "u.password_expired = 'Y'".to_string(),
                false => no.into(),
            },
            none.into(),
            none.into(),
            "",
        ),
    };
    let flag = |condition: &str| match condition {
        "'n'" => "'n'".to_string(),
        condition => format!("CASE WHEN {condition} THEN 'y' ELSE 'n' END"),
    };
    format!(
        "SELECT u.User AS user, u.Host AS host, {} AS superuser, {} AS has_password, \
         {} AS locked, {} AS is_role, {} AS expired, {expires} AS expires, \
         {default_role} AS default_role, u.max_user_connections AS connection_limit \
         FROM mysql.user AS u {join} ORDER BY u.User, u.Host",
        flag(&superuser),
        flag(&password),
        flag(&locked),
        flag(&role),
        flag(&expired),
    )
}

const ROLE_EDGES_SQL: &str = "SELECT FROM_USER AS role_user, FROM_HOST AS role_host, \
     TO_USER AS member_user, TO_HOST AS member_host FROM mysql.role_edges";

const DEFAULT_ROLES_SQL: &str = "SELECT USER AS member_user, HOST AS member_host, \
     DEFAULT_ROLE_USER AS role_user, DEFAULT_ROLE_HOST AS role_host FROM mysql.default_roles";

const ROLES_MAPPING_SQL: &str =
    "SELECT User AS member_user, Host AS member_host, Role AS role_user FROM mysql.roles_mapping";

const DATABASES_SQL: &str =
    "SELECT SCHEMA_NAME AS name FROM information_schema.SCHEMATA ORDER BY 1";

/// Sessions by user. `PROCESSLIST` shows a login without `PROCESS` its own
/// sessions alone, so the count is what this login may see. It names the
/// user, not the account its host matched.
const SESSIONS_SQL: &str = "SELECT USER AS name, COUNT(*) AS sessions \
     FROM information_schema.PROCESSLIST \
     WHERE USER NOT IN ('system user', 'event_scheduler', 'unauthenticated user') \
     GROUP BY USER ORDER BY COUNT(*) DESC, USER";

/// How many tables each of `databases` holds.
pub(super) fn tables_sql(engine: Engine, databases: &[String]) -> String {
    let names: Vec<String> = databases.iter().map(|d| engine.quote_literal(d)).collect();
    format!(
        "SELECT TABLE_SCHEMA AS name, COUNT(*) AS tables FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA IN ({}) GROUP BY TABLE_SCHEMA",
        names.join(", ")
    )
}

fn privilege_columns() -> String {
    (TABLE_PRIVILEGES.iter())
        .map(|(_, column)| *column)
        .chain(["Grant_priv"])
        .collect::<Vec<_>>()
        .join(", ")
}

/// Who holds a privilege on every table: `mysql.user`'s global columns.
/// Every global grant, with MySQL's partial revokes where the server keeps
/// them (`restrictions`): `User_attributes`' `Restrictions`, a JSON list of
/// `{"Database": …, "Privileges": […]}`.
fn global_grants_sql(restrictions: bool) -> String {
    let restricted = match restrictions {
        true => ", JSON_EXTRACT(User_attributes, '$.Restrictions') AS restrictions",
        false => "",
    };
    format!(
        "SELECT User AS user, Host AS host, {}{restricted} FROM mysql.user ORDER BY User, Host",
        privilege_columns()
    )
}

/// The server version partial revokes came in, and with them schema names
/// in grants read literally while `partial_revokes` is on.
const PARTIAL_REVOKES: (u16, u16, u16) = (8, 0, 16);

/// The privileges a `mysql.user` row's `restrictions` take off `database`.
fn revoked_on(restrictions: &str, database: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Restriction {
        #[serde(rename = "Database")]
        database: String,
        #[serde(rename = "Privileges")]
        privileges: Vec<String>,
    }
    let read: Vec<Restriction> = serde_json::from_str(restrictions).unwrap_or_default();
    (read.into_iter())
        .filter(|restriction| restriction.database == database)
        .flat_map(|restriction| restriction.privileges)
        .map(|privilege| privilege.to_uppercase())
        .collect()
}

/// Every database-level grant: which databases each reaches is decided
/// here, by `LIKE`, since the server keeps patterns in `Db` as written.
fn database_grants_sql() -> String {
    format!(
        "SELECT Db AS db, User AS user, Host AS host, {} FROM mysql.db ORDER BY User, Host",
        privilege_columns()
    )
}

fn table_grants_sql(engine: Engine, schema: &str, name: &str) -> String {
    format!(
        "SELECT User AS user, Host AS host, Table_priv AS privileges FROM mysql.tables_priv \
         WHERE Db = {} AND Table_name = {} ORDER BY User, Host",
        engine.quote_literal(schema),
        engine.quote_literal(name)
    )
}

fn column_grants_sql(engine: Engine, schema: &str, name: &str) -> String {
    format!(
        "SELECT User AS user, Host AS host, Column_name AS name, Column_priv AS privileges \
         FROM mysql.columns_priv WHERE Db = {} AND Table_name = {} \
         ORDER BY User, Host, Column_name",
        engine.quote_literal(schema),
        engine.quote_literal(name)
    )
}

fn yes(result: &QueryResult, row: &Row, column: &str) -> Result<bool, DbError> {
    match required_cell(result, row, column)? {
        "y" | "Y" => Ok(true),
        "n" | "N" => Ok(false),
        other => Err(super::unexpected_catalog_value(column, other)),
    }
}

/// The text of a cell that may be empty, as a grant table's host is for a
/// MariaDB role.
fn cell<'a>(result: &'a QueryResult, row: &'a Row, column: &str) -> &'a str {
    optional_cell(result, row, column).unwrap_or_default()
}

/// [`accounts_sql`]'s answer, with what each account's `default_role` is
/// (MariaDB's, by role name).
pub(super) fn assemble_accounts(
    engine: Engine,
    accounts: &QueryResult,
) -> Result<(Vec<Role>, HashMap<Account, String>), DbError> {
    let mut roles = Vec::new();
    let mut defaults = HashMap::new();
    for row in &accounts.rows {
        let account = Account::of(
            engine,
            required_cell(accounts, row, "user")?,
            cell(accounts, row, "host"),
        );
        let connection_limit = match optional_cell(accounts, row, "connection_limit") {
            Some(limit) => limit
                .parse::<i64>()
                .map_err(|_| super::unexpected_catalog_value("connection limit", limit))?,
            None => 0,
        };
        if let Some(role) = optional_cell(accounts, row, "default_role") {
            defaults.insert(account.clone(), role.to_string());
        }
        roles.push(Role {
            name: account.display(),
            // A MySQL 8 role is an account made locked, so it reads as one
            // that cannot log in, which is what it is; a role left unlocked
            // can log in, and is shown so.
            login: !yes(accounts, row, "locked")? && !yes(accounts, row, "is_role")?,
            superuser: yes(accounts, row, "superuser")?,
            password_expires: optional_cell(accounts, row, "expires").map(str::to_string),
            expired: yes(accounts, row, "expired")?,
            connection_limit: (connection_limit > 0).then_some(connection_limit),
            has_password: Some(yes(accounts, row, "has_password")?),
            last_login: None,
        });
    }
    Ok((roles, defaults))
}

/// [`Roles::memberships`] and [`Roles::not_inherited`], as read.
type Memberships = (Vec<(String, String)>, Vec<(String, String)>);

/// MySQL 8's memberships: `role_edges`, and which of them are active at
/// login, `default_roles`. A role granted but not a default one is not
/// active until `SET ROLE`, which is what not inheriting means here, unless
/// the server activates every role at login.
pub(super) fn assemble_edges(
    edges: &QueryResult,
    defaults: &QueryResult,
    activate_all: bool,
) -> Result<Memberships, DbError> {
    let pair = |result: &QueryResult, row: &Row| -> Result<(String, String), DbError> {
        Ok((
            Account::of(
                Engine::MySql,
                required_cell(result, row, "member_user")?,
                cell(result, row, "member_host"),
            )
            .display(),
            Account::of(
                Engine::MySql,
                required_cell(result, row, "role_user")?,
                cell(result, row, "role_host"),
            )
            .display(),
        ))
    };
    let memberships = (edges.rows.iter())
        .map(|row| pair(edges, row))
        .collect::<Result<Vec<_>, _>>()?;
    let active = (defaults.rows.iter())
        .map(|row| pair(defaults, row))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let not_inherited = match activate_all {
        true => Vec::new(),
        false => (memberships.iter())
            .filter(|membership| !active.contains(*membership))
            .cloned()
            .collect(),
    };
    Ok((memberships, not_inherited))
}

/// MariaDB's memberships, `roles_mapping`. A role granted to a role is
/// always in effect; one granted to a user only while it is the user's
/// default role, or after `SET ROLE`.
pub(super) fn assemble_mapping(
    mapping: &QueryResult,
    defaults: &HashMap<Account, String>,
) -> Result<Memberships, DbError> {
    let mut memberships = Vec::new();
    let mut not_inherited = Vec::new();
    for row in &mapping.rows {
        let member = Account::of(
            Engine::MariaDb,
            required_cell(mapping, row, "member_user")?,
            cell(mapping, row, "member_host"),
        );
        let role = required_cell(mapping, row, "role_user")?.to_string();
        let pair = (member.display(), role.clone());
        if member.host.is_some() && defaults.get(&member) != Some(&role) {
            not_inherited.push(pair.clone());
        }
        memberships.push(pair);
    }
    Ok((memberships, not_inherited))
}

/// What one account holds, line by line, each with the role it is held
/// through (`None` for its own).
pub(super) type Held = Vec<(Option<String>, Line)>;

/// Whether the privileges held on `*.*` make a superuser
/// ([`SUPERUSER_PRIVILEGES`] with grant option, or all of them).
fn holds_everything(held: &Held) -> bool {
    let global: Vec<(&Privilege, bool)> = (held.iter())
        .filter_map(|(_, line)| match line {
            Line::Privileges {
                privileges,
                level: Level::Global,
                grantable,
                ..
            } => Some(
                privileges
                    .iter()
                    .map(move |privilege| (privilege, *grantable)),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    let grantable = global.iter().any(|(_, grantable)| *grantable);
    let holds = |name: &str| global.iter().any(|(privilege, _)| privilege.name == name);
    grantable && (holds("ALL PRIVILEGES") || SUPERUSER_PRIVILEGES.iter().all(|name| holds(name)))
}

fn source_of(through: &Option<String>, own: Source) -> Source {
    match through {
        Some(role) => Source::Via(role.clone()),
        None => own,
    }
}

fn add_right(rights: &mut Vec<Right>, privilege: &str, source: Source) {
    match rights.iter_mut().find(|right| right.privilege == privilege) {
        Some(right) if right.sources.contains(&source) => {}
        Some(right) => right.sources.push(source),
        None => rights.push(Right {
            privilege: privilege.to_string(),
            sources: vec![source],
        }),
    }
}

fn privilege_rank(name: &str) -> usize {
    (DATA_PRIVILEGES.iter())
        .position(|known| *known == name)
        .unwrap_or(DATA_PRIVILEGES.len())
}

/// The names of `privileges`, with `GRANT OPTION` where the grant gives it.
fn named(privileges: &[Privilege], grantable: bool) -> Vec<String> {
    let mut names: Vec<String> = privileges.iter().map(|p| p.name.clone()).collect();
    if grantable && !names.iter().any(|name| name == "GRANT OPTION") {
        names.push("GRANT OPTION".into());
    }
    names
}

/// What is held on the tables and routines of `database`.
fn held_inside(held: &Held, database: &str) -> Vec<HeldGrant> {
    let mut inside = Vec::new();
    for (through, line) in held {
        let Line::Privileges {
            privileges,
            level,
            grantable,
            ..
        } = line
        else {
            continue;
        };
        let relation = |privilege: &Privilege| match level {
            Level::Table {
                database: db,
                table,
            } if db == database => Some(match privilege.columns.as_slice() {
                [] => table.clone(),
                columns => {
                    let mut columns = columns.to_vec();
                    columns.sort_unstable();
                    format!("{table} ({})", columns.join(", "))
                }
            }),
            Level::Routine {
                database: db,
                name,
                kind,
            } if db == database => Some(format!("{name} ({kind})")),
            _ => None,
        };
        let mut privileges = privileges.clone();
        if privileges.is_empty() && *grantable {
            privileges.push(Privilege {
                name: "GRANT OPTION".into(),
                columns: Vec::new(),
            });
        }
        for privilege in &privileges {
            let Some(relation) = relation(privilege) else {
                continue;
            };
            inside.push(HeldGrant {
                schema: database.to_string(),
                relation,
                privilege: privilege.name.clone(),
                grantable: *grantable,
                through: through.clone(),
                usable: true,
                sequence: false,
            });
        }
    }
    inside
}

/// A role's access to every database `databases` lists, from what
/// [`Held`] says it holds: the databases it holds anything in, then a
/// section for each database a grant names that is not listed, so no grant
/// goes unshown.
pub(super) fn assemble_access(held: &Held, databases: &[String], superuser: bool) -> RoleAccess {
    let mut server = Vec::new();
    let mut global: Vec<Right> = Vec::new();
    let mut revokes: Vec<Revoked> = Vec::new();
    for (through, line) in held {
        let suffix = match through {
            Some(role) => format!(" through {role}"),
            None => String::new(),
        };
        match line {
            Line::Privileges {
                privileges,
                level: Level::Global,
                grantable,
                ..
            } => {
                for name in named(privileges, *grantable) {
                    if name == "ALL PRIVILEGES" || !DATA_PRIVILEGES.contains(&name.as_str()) {
                        let grantable = match *grantable && name != "GRANT OPTION" {
                            true => " (grantable)",
                            false => "",
                        };
                        server.push(format!("{name}{grantable}{suffix}"));
                    }
                    if DATA_PRIVILEGES.contains(&name.as_str()) {
                        add_right(&mut global, &name, source_of(through, Source::Global));
                    }
                }
            }
            Line::Proxy {
                target, grantable, ..
            } => {
                let grantable = if *grantable { " (grantable)" } else { "" };
                server.push(format!(
                    "PROXY ON {}{grantable}{suffix}",
                    target.shown_quoted()
                ));
            }
            Line::Revoke {
                privileges,
                database,
                ..
            } => revokes.push(Revoked {
                source: source_of(through, Source::Global),
                database: database.clone(),
                privileges: named(privileges, false),
            }),
            Line::Unread(text) => server.push(format!("unread: {text}")),
            Line::Privileges { .. } | Line::Roles { .. } | Line::Skipped => {}
        }
    }
    server.dedup();

    let mut names: Vec<(String, bool)> = databases.iter().map(|d| (d.clone(), true)).collect();
    for (_, line) in held {
        let named = match line {
            Line::Privileges {
                level: Level::Database(name),
                ..
            } => name,
            Line::Privileges {
                level: Level::Table { database, .. } | Level::Routine { database, .. },
                ..
            } => database,
            _ => continue,
        };
        let reaches = |listed: &String| match line {
            Line::Privileges {
                level: Level::Database(_),
                ..
            } => database_source(named, listed).is_some(),
            _ => listed == named,
        };
        if !databases.iter().any(reaches) && !names.iter().any(|(name, _)| name == named) {
            names.push((named.clone(), false));
        }
    }

    let mut access = Vec::new();
    for (name, listed) in names {
        let mut rights = match listed {
            true => restricted(&global, &revokes, &name),
            false => Vec::new(),
        };
        for (through, line) in held {
            let Line::Privileges {
                privileges,
                level: Level::Database(granted),
                grantable,
                ..
            } = line
            else {
                continue;
            };
            let own = match listed {
                true => database_source(granted, &name),
                false => (*granted == name).then(|| match is_pattern(granted) {
                    true => Source::Pattern(format!("{granted}.*")),
                    false => Source::Direct,
                }),
            };
            let Some(own) = own else {
                continue;
            };
            for privilege in named(privileges, *grantable) {
                add_right(&mut rights, &privilege, source_of(through, own.clone()));
            }
        }
        for right in &mut rights {
            right.sources.sort_by_key(Source::rank);
        }
        rights.sort_by_key(|right| privilege_rank(&right.privilege));
        let inside = held_inside(held, &name);
        if rights.is_empty() && inside.is_empty() {
            continue;
        }
        access.push(DatabaseAccess {
            name: name.clone(),
            allows_connections: true,
            rights,
            inside: Some(RoleGrants {
                database: name,
                schemas: Vec::new(),
                held: inside,
                beyond: Beyond {
                    superuser,
                    ..Beyond::default()
                },
                flat: true,
            }),
        });
    }
    RoleAccess {
        superuser,
        databases: access,
        server,
    }
}

/// A partial revoke: `privileges` of a global grant, held as `source`,
/// taken off `database`.
struct Revoked {
    source: Source,
    database: String,
    privileges: Vec<String>,
}

/// The global rights `global` as they reach `database`, less what a partial
/// revoke takes off it there. `ALL PRIVILEGES` with any of it revoked is
/// what is left of it, privilege by privilege.
fn restricted(global: &[Right], revokes: &[Revoked], database: &str) -> Vec<Right> {
    let here: Vec<&Revoked> = (revokes.iter())
        .filter(|revoke| revoke.database == database)
        .collect();
    if here.is_empty() {
        return global.to_vec();
    }
    let mut rights = Vec::new();
    for right in global {
        for source in &right.sources {
            let revoked: Vec<&str> = (here.iter())
                .filter(|revoke| revoke.source == *source)
                .flat_map(|revoke| revoke.privileges.iter().map(String::as_str))
                .collect();
            let gone = |privilege: &str| {
                revoked.contains(&privilege) || revoked.contains(&"ALL PRIVILEGES")
            };
            if right.privilege == "ALL PRIVILEGES" && !revoked.is_empty() {
                let left = (DATA_PRIVILEGES.iter())
                    .filter(|privilege| !matches!(**privilege, "ALL PRIVILEGES" | "GRANT OPTION"));
                for privilege in left.filter(|privilege| !gone(privilege)) {
                    add_right(&mut rights, privilege, source.clone());
                }
            } else if !gone(&right.privilege) {
                add_right(&mut rights, &right.privilege, source.clone());
            }
        }
    }
    rights
}

/// Whether `privilege` acts on a table, and so is counted in tables.
fn on_tables(privilege: &str) -> bool {
    privilege == "ALL PRIVILEGES" || TABLE_PRIVILEGES.iter().any(|(name, _)| *name == privilege)
}

/// The databases among `databases` [`Held`]'s database-level grants reach
/// on tables, whose tables [`assemble_reach`] needs counted.
pub(super) fn reached_databases(held: &Held, databases: &[String]) -> Vec<String> {
    (databases.iter())
        .filter(|database| {
            held.iter().any(|(_, line)| match line {
                Line::Privileges {
                    level: Level::Database(granted),
                    privileges,
                    ..
                } => {
                    privileges.iter().any(|p| on_tables(&p.name))
                        && database_source(granted, database).is_some()
                }
                _ => false,
            })
        })
        .cloned()
        .collect()
}

/// What one database's privilege on tables covers, for the reach.
#[derive(Default)]
struct Covered {
    /// A database-level grant: every table of the database.
    every: bool,
    tables: BTreeSet<String>,
    through: BTreeSet<String>,
}

fn covering<'a>(
    covered: &'a mut BTreeMap<(String, usize, String), Covered>,
    database: &str,
    privilege: &str,
    through: &Option<String>,
) -> &'a mut Covered {
    let key = (
        database.to_string(),
        privilege_rank(privilege),
        privilege.to_string(),
    );
    let entry = covered.entry(key).or_default();
    entry.through.extend(through.clone());
    entry
}

/// The detail panel's summary of [`Held`]: per database and privilege on
/// tables, how many tables it covers (a database-level grant all the
/// database's, `tables` says how many), and what `*.*`, a proxy or an
/// unread line gives, said as such. A database-level grant naming no
/// listed database counts nothing and is left to Show Grants.
pub(super) fn assemble_reach(
    held: &Held,
    databases: &[String],
    tables: &HashMap<String, u64>,
) -> Reach {
    let mut covered: BTreeMap<(String, usize, String), Covered> = BTreeMap::new();
    let mut predefined: Vec<(Option<String>, Vec<String>)> = Vec::new();
    let mut other = Vec::new();
    for (through, line) in held {
        match line {
            Line::Privileges {
                privileges,
                level,
                grantable,
                ..
            } => match level {
                Level::Global => {
                    let names = named(privileges, *grantable);
                    match predefined.iter_mut().find(|(by, _)| by == through) {
                        Some((_, all)) => all.extend(names),
                        None => predefined.push((through.clone(), names)),
                    }
                }
                Level::Database(granted) => {
                    for database in databases {
                        if database_source(granted, database).is_none() {
                            continue;
                        }
                        for privilege in privileges.iter().filter(|p| on_tables(&p.name)) {
                            covering(&mut covered, database, &privilege.name, through).every = true;
                        }
                    }
                }
                Level::Table { database, table } => {
                    for privilege in privileges.iter().filter(|p| on_tables(&p.name)) {
                        let name = match privilege.columns.is_empty() {
                            true => privilege.name.clone(),
                            false => format!("{} (columns)", privilege.name),
                        };
                        (covering(&mut covered, database, &name, through).tables)
                            .insert(table.clone());
                    }
                }
                Level::Routine { .. } => {}
            },
            Line::Proxy {
                target, grantable, ..
            } => {
                let grantable = if *grantable { " (grantable)" } else { "" };
                let route = match through {
                    Some(role) => format!(" through {role}"),
                    None => String::new(),
                };
                other.push(format!(
                    "PROXY ON {}{grantable}{route}",
                    target.shown_quoted()
                ));
            }
            Line::Revoke {
                privileges,
                database,
                ..
            } => {
                let route = match through {
                    Some(role) => format!(" through {role}"),
                    None => String::new(),
                };
                other.push(format!(
                    "{} revoked on {database}.*{route}",
                    named(privileges, false).join(", ")
                ));
            }
            Line::Unread(text) => other.push(format!("unread: {text}")),
            Line::Roles { .. } | Line::Skipped => {}
        }
    }
    let entries = (covered.into_iter())
        .map(|((database, _, privilege), covered)| ReachEntry {
            relations: match covered.every {
                true => tables.get(&database).copied().unwrap_or(0),
                false => covered.tables.len() as u64,
            },
            schema: database,
            privilege,
            through: covered.through.into_iter().collect(),
        })
        .collect();
    let mut summary: Vec<String> = (predefined.into_iter())
        .filter(|(_, names)| !names.is_empty())
        .map(|(through, names)| match through {
            Some(role) => format!("{} on *.* through {role}", names.join(", ")),
            None => format!("{} on *.*", names.join(", ")),
        })
        .collect();
    summary.extend(other);
    Reach {
        database: String::new(),
        entries,
        owns: 0,
        predefined: summary,
    }
}

/// A grant table's `SET` of privileges, `Select,Show view,Grant`, upper
/// case, and whether `Grant` was among them.
fn set_privileges(set: &str) -> (Vec<String>, bool) {
    let mut grantable = false;
    let mut privileges = Vec::new();
    for privilege in set.split(',').filter(|p| !p.is_empty()) {
        match privilege.eq_ignore_ascii_case("Grant") {
            true => grantable = true,
            false => privileges.push(privilege.to_uppercase()),
        }
    }
    (privileges, grantable)
}

fn table_rank(privilege: &str) -> usize {
    let bare = privilege.split(" (").next().unwrap_or(privilege);
    (TABLE_PRIVILEGES.iter())
        .position(|(name, _)| *name == bare)
        .unwrap_or(TABLE_PRIVILEGES.len())
}

/// Who holds what on `schema.name`: grants on the table itself and on its
/// columns, then the database-level grants that reach it (by name or
/// pattern) and the global ones, each saying where it is written.
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_object_grants(
    engine: Engine,
    schema: &str,
    literal_names: bool,
    global: &QueryResult,
    databases: &QueryResult,
    table: &QueryResult,
    columns: &QueryResult,
    roles: Roles,
) -> Result<ObjectGrants, DbError> {
    let account = |result: &QueryResult, row: &Row| -> Result<String, DbError> {
        Ok(Account::of(
            engine,
            required_cell(result, row, "user")?,
            cell(result, row, "host"),
        )
        .display())
    };
    let mut grants = Vec::new();
    let mut table_grantable = BTreeSet::new();
    for row in &table.rows {
        let grantee = account(table, row)?;
        let (privileges, grantable) = set_privileges(cell(table, row, "privileges"));
        if grantable {
            table_grantable.insert(grantee.clone());
        }
        for privilege in privileges {
            grants.push(Grant {
                grantee: Some(grantee.clone()),
                privilege,
                grantable,
                on: None,
            });
        }
    }
    let mut by_column: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for row in &columns.rows {
        let grantee = account(columns, row)?;
        let column = required_cell(columns, row, "name")?;
        for privilege in set_privileges(cell(columns, row, "privileges")).0 {
            (by_column.entry((grantee.clone(), privilege)).or_default()).push(column.to_string());
        }
    }
    for ((grantee, privilege), mut names) in by_column {
        names.sort_unstable();
        grants.push(Grant {
            grantable: table_grantable.contains(&grantee),
            grantee: Some(grantee),
            privilege: format!("{privilege} ({})", names.join(", ")),
            on: None,
        });
    }
    let levels = [(databases, true), (global, false)];
    for (result, per_database) in levels {
        for row in &result.rows {
            let on = match per_database {
                true => {
                    let granted = required_cell(result, row, "db")?;
                    let read = match literal_names {
                        true => literal(granted),
                        false => granted.to_string(),
                    };
                    if database_source(&read, schema).is_none() {
                        continue;
                    }
                    format!("{granted}.*")
                }
                false => "*.*".to_string(),
            };
            let grantee = account(result, row)?;
            let grantable = yes(result, row, "Grant_priv")?;
            // A partial revoke takes a global privilege off this database.
            let revoked = match per_database {
                true => Vec::new(),
                false => revoked_on(cell(result, row, "restrictions"), schema),
            };
            for (privilege, column) in TABLE_PRIVILEGES {
                if revoked.iter().any(|gone| gone == privilege) {
                    continue;
                }
                if yes(result, row, column)? {
                    grants.push(Grant {
                        grantee: Some(grantee.clone()),
                        privilege: privilege.to_string(),
                        grantable,
                        on: Some(on.clone()),
                    });
                }
            }
        }
    }
    grants.sort_by(|a, b| {
        (a.grantee.cmp(&b.grantee)).then(table_rank(&a.privilege).cmp(&table_rank(&b.privilege)))
    });
    Ok(ObjectGrants {
        owner: String::new(),
        privileges: TABLE_PRIVILEGES
            .iter()
            .map(|(name, _)| name.to_string())
            .collect(),
        grants,
        roles,
        sequence: false,
        public: false,
    })
}

/// The audit lists: no login is recorded, so idle logins are unknown. MySQL
/// has no PUBLIC, so PUBLIC's grants are none; MariaDB has one from 10.11
/// (`has_public`), whose grants are not read, so they are unknown rather
/// than none. A role is unused when no account holds it.
pub(super) fn assemble_reports(
    roles: &Roles,
    sessions: &QueryResult,
    has_public: bool,
) -> Result<Reports, DbError> {
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
    let login = |role: &Role| role.login || role.deactivated();
    Ok(Reports {
        superusers: (roles.roles.iter())
            .filter(|role| role.superuser)
            .map(|role| role.name.clone())
            .collect(),
        idle: None,
        expired: (roles.roles.iter())
            .filter(|role| login(role) && role.expired)
            .map(|role| role.name.clone())
            .collect(),
        public_grants: (!has_public).then(Vec::new),
        unused_roles: (roles.roles.iter())
            .filter(|role| !login(role) && roles.members(&role.name).is_empty())
            .map(|role| role.name.clone())
            .collect(),
        sessions: by_login,
    })
}

fn names(result: &QueryResult, column: &str) -> Result<Vec<String>, DbError> {
    (result.rows.iter())
        .map(|row| required_cell(result, row, column).map(str::to_string))
        .collect()
}

impl super::mysql::Connection {
    /// `sql`'s rows, or `None` where the server refused it for want of a
    /// privilege: asked first with `LIMIT 0`, since a refusal comes back as
    /// a code only on the driver's own error.
    fn read_unless_denied(&self, sql: &str) -> Result<Option<QueryResult>, DbError> {
        if self.refuses(&format!("{sql} LIMIT 0"))? {
            return Ok(None);
        }
        self.internal_query(sql).map(Some)
    }

    pub fn roles(&self) -> Result<Roles, DbError> {
        let engine = self.engine();
        let version = self.server_version()?;
        let Some(accounts) = self.read_unless_denied(&accounts_sql(engine, version))? else {
            return Ok(Roles {
                denied: Some(DENIED.into()),
                ..Roles::default()
            });
        };
        let (roles, defaults) = assemble_accounts(engine, &accounts)?;
        // Memberships the login may not read leave the accounts listed
        // without them, rather than the whole list refused.
        let (memberships, not_inherited) = match engine {
            Engine::MariaDb if has_roles(engine, version) => {
                match self.read_unless_denied(ROLES_MAPPING_SQL)? {
                    Some(mapping) => assemble_mapping(&mapping, &defaults)?,
                    None => Default::default(),
                }
            }
            _ if has_roles(engine, version) => {
                let edges = self.read_unless_denied(ROLE_EDGES_SQL)?;
                let active = self.read_unless_denied(DEFAULT_ROLES_SQL)?;
                match (edges, active) {
                    (Some(edges), Some(active)) => {
                        let everything = self.internal_query(
                            "SELECT IF(@@activate_all_roles_on_login, 'y', 'n') AS everything",
                        )?;
                        let row = everything.rows.first().cloned().unwrap_or_default();
                        assemble_edges(&edges, &active, yes(&everything, &row, "everything")?)?
                    }
                    _ => Default::default(),
                }
            }
            _ => Default::default(),
        };
        Ok(Roles {
            roles,
            memberships,
            not_inherited,
            records_logins: false,
            denied: None,
        })
    }

    /// Everything `name` holds, its own grants and, transitively, those of
    /// every role it holds active as it logs in; a role granted twice is
    /// read once. MariaDB's `SHOW GRANTS` of an account repeats
    /// its roles' lines under their own names, which are left for each
    /// role's own read.
    fn held(&self, name: &str) -> Result<Held, DbError> {
        let roles = has_roles(self.engine(), self.server_version()?);
        let start = Account::parse(name);
        // A login's roles count only where they are active as it logs in:
        // one it must `SET ROLE` to is a membership marked not inherited,
        // and its grants are none of the login's own reach. A role active
        // has every role granted to it active too.
        let memberships = match roles {
            true => Some(self.roles()?),
            false => None,
        };
        let login = (memberships.as_ref())
            .and_then(|read| read.role(&start.display()))
            .is_some_and(|role| role.login);
        let mut queue = vec![(start.clone(), None)];
        let mut seen = vec![start.clone()];
        let mut held = Vec::new();
        let mut index = 0;
        while let Some((account, through)) = queue.get(index).cloned() {
            index += 1;
            let lines = self.internal_query(&format!("SHOW GRANTS FOR {}", account.quoted()))?;
            for row in &lines.rows {
                let line = parse_grant(row.get(0).unwrap_or_default());
                if line.grantee().is_some_and(|grantee| *grantee != account) {
                    continue;
                }
                if let (true, Line::Roles { roles: granted, .. }) = (roles, &line) {
                    let active = |role: &Account| {
                        !(login && account == start)
                            || (memberships.as_ref())
                                .is_none_or(|read| read.inherits(&start.display(), &role.display()))
                    };
                    for role in granted.iter().filter(|role| active(role)) {
                        if !seen.contains(role) {
                            seen.push(role.clone());
                            queue.push((role.clone(), Some(role.display())));
                        }
                    }
                }
                held.push((through.clone(), line));
            }
        }
        Ok(match self.literal_schema_names()? {
            true => literal_levels(held),
            false => held,
        })
    }

    /// Whether the server reads a `_` or `%` in a schema-level grant as
    /// itself: MySQL from 8.0.16 with `partial_revokes` on, which also lists
    /// the partial revokes in `SHOW GRANTS` and `mysql.user`.
    fn literal_schema_names(&self) -> Result<bool, DbError> {
        if self.engine() != Engine::MySql || self.server_version()? < PARTIAL_REVOKES {
            return Ok(false);
        }
        let read = self.internal_query("SELECT @@GLOBAL.partial_revokes AS literal")?;
        Ok((read.rows.first()).is_some_and(|row| cell(&read, row, "literal") == "1"))
    }

    pub fn role_access(&self, name: &str) -> Result<RoleAccess, DbError> {
        let held = self.held(name)?;
        let databases = names(&self.internal_query(DATABASES_SQL)?, "name")?;
        let superuser = holds_everything(&held);
        Ok(assemble_access(&held, &databases, superuser))
    }

    pub fn role_reach(&self, name: &str) -> Result<Reach, DbError> {
        let held = self.held(name)?;
        let databases = names(&self.internal_query(DATABASES_SQL)?, "name")?;
        let reached = reached_databases(&held, &databases);
        let mut tables = HashMap::new();
        if !reached.is_empty() {
            let counts = self.internal_query(&tables_sql(self.engine(), &reached))?;
            for row in &counts.rows {
                let count = required_cell(&counts, row, "tables")?;
                tables.insert(
                    required_cell(&counts, row, "name")?.to_string(),
                    count
                        .parse()
                        .map_err(|_| super::unexpected_catalog_value("count", count))?,
                );
            }
        }
        Ok(assemble_reach(&held, &databases, &tables))
    }

    /// Never asked on MySQL: [`Self::role_access`] already carries what is
    /// inside every database ([`DatabaseAccess::inside`]).
    pub fn role_grants(&self, _name: &str) -> Result<RoleGrants, DbError> {
        Err(plain_error(
            "A MySQL database's grants come with the role's access to every database.".into(),
        ))
    }

    pub fn object_grants(&self, schema: &str, name: &str) -> Result<ObjectGrants, DbError> {
        let engine = self.engine();
        let denied = || plain_error(GRANTS_DENIED.into());
        let literal_names = self.literal_schema_names()?;
        let restrictions = engine == Engine::MySql && self.server_version()? >= PARTIAL_REVOKES;
        let global = self
            .read_unless_denied(&global_grants_sql(restrictions))?
            .ok_or_else(denied)?;
        let databases = (self.read_unless_denied(&database_grants_sql())?).ok_or_else(denied)?;
        let table = (self.read_unless_denied(&table_grants_sql(engine, schema, name))?)
            .ok_or_else(denied)?;
        let columns = (self.read_unless_denied(&column_grants_sql(engine, schema, name))?)
            .ok_or_else(denied)?;
        let roles = self.roles()?;
        assemble_object_grants(
            engine,
            schema,
            literal_names,
            &global,
            &databases,
            &table,
            &columns,
            roles,
        )
    }

    pub fn role_reports(&self, roles: &Roles) -> Result<Reports, DbError> {
        let has_public = self.engine() == Engine::MariaDb && self.server_version()? >= (10, 11, 0);
        assemble_reports(roles, &self.internal_query(SESSIONS_SQL)?, has_public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(user: &str, host: &str) -> Account {
        Account {
            user: user.into(),
            host: Some(host.into()),
        }
    }

    fn role(name: &str) -> Account {
        Account {
            user: name.into(),
            host: None,
        }
    }

    fn privilege(name: &str) -> Privilege {
        Privilege {
            name: name.into(),
            columns: Vec::new(),
        }
    }

    fn privileges(names: &[&str], level: Level, grantee: Account, grantable: bool) -> Line {
        Line::Privileges {
            privileges: names.iter().map(|name| privilege(name)).collect(),
            level,
            grantee,
            grantable,
        }
    }

    fn database(name: &str) -> Level {
        Level::Database(name.into())
    }

    fn table(database: &str, table: &str) -> Level {
        Level::Table {
            database: database.into(),
            table: table.into(),
        }
    }

    const HASH: &str = "*DB14CBAE92D7CB2F84BD3AA7222415B564A4054A";

    #[test]
    fn mysql_56_grants_read_with_their_password_tails_dropped() {
        let app = || user("app", "%");
        assert_eq!(
            parse_grant(&format!(
                "GRANT USAGE ON *.* TO 'app'@'%' IDENTIFIED BY PASSWORD '{HASH}'"
            )),
            privileges(&[], Level::Global, app(), false)
        );
        assert_eq!(
            parse_grant("GRANT SELECT, INSERT ON `shop`.* TO 'app'@'%'"),
            privileges(&["SELECT", "INSERT"], database("shop"), app(), false)
        );
        assert_eq!(
            parse_grant("GRANT UPDATE (total, note) ON `shop`.`orders` TO 'app'@'%'"),
            Line::Privileges {
                privileges: vec![Privilege {
                    name: "UPDATE".into(),
                    columns: vec!["total".into(), "note".into()],
                }],
                level: table("shop", "orders"),
                grantee: app(),
                grantable: false,
            }
        );
        assert_eq!(
            parse_grant("GRANT EXECUTE ON PROCEDURE `shop`.`restock` TO 'app'@'%'"),
            privileges(
                &["EXECUTE"],
                Level::Routine {
                    database: "shop".into(),
                    name: "restock".into(),
                    kind: "procedure",
                },
                app(),
                false
            )
        );
        let reader = || user("reader", "10.0.0.%");
        // What 5.6 sends, one backslash: the client's display doubles it.
        assert_eq!(
            parse_grant(r"GRANT SELECT ON `db1\_%`.* TO 'reader'@'10.0.0.%'"),
            privileges(&["SELECT"], database(r"db1\_%"), reader(), false)
        );
        assert_eq!(
            parse_grant("GRANT SELECT ON `shop`.`items` TO 'reader'@'10.0.0.%' WITH GRANT OPTION"),
            privileges(&["SELECT"], table("shop", "items"), reader(), true)
        );
        assert_eq!(
            parse_grant(
                "GRANT SELECT, RELOAD, PROCESS ON *.* TO 'ops'@'localhost' \
                 IDENTIFIED BY PASSWORD '*A5C6...'"
            ),
            privileges(
                &["SELECT", "RELOAD", "PROCESS"],
                Level::Global,
                user("ops", "localhost"),
                false
            )
        );
        assert_eq!(
            parse_grant("GRANT PROXY ON 'app'@'%' TO 'ops'@'localhost'"),
            Line::Proxy {
                target: app(),
                grantee: user("ops", "localhost"),
                grantable: false,
            }
        );
        assert_eq!(
            parse_grant("GRANT SELECT ON `shop`.* TO 'odd@name'@'%'"),
            privileges(&["SELECT"], database("shop"), user("odd@name", "%"), false)
        );
    }

    #[test]
    fn mysql_8_grants_read_backticked_accounts_roles_and_dynamic_privileges() {
        let app = || user("app", "%");
        assert_eq!(
            parse_grant("GRANT USAGE ON *.* TO `app`@`%`"),
            privileges(&[], Level::Global, app(), false)
        );
        assert_eq!(
            parse_grant("GRANT UPDATE (`note`, `total`) ON `shop`.`orders` TO `app`@`%`"),
            Line::Privileges {
                privileges: vec![Privilege {
                    name: "UPDATE".into(),
                    columns: vec!["note".into(), "total".into()],
                }],
                level: table("shop", "orders"),
                grantee: app(),
                grantable: false,
            }
        );
        assert_eq!(
            parse_grant("GRANT `shop_write`@`%` TO `app`@`%`"),
            Line::Roles {
                roles: vec![user("shop_write", "%")],
                grantee: app(),
            }
        );
        assert_eq!(
            parse_grant("GRANT BACKUP_ADMIN ON *.* TO `ops`@`localhost`"),
            privileges(
                &["BACKUP_ADMIN"],
                Level::Global,
                user("ops", "localhost"),
                false
            )
        );
        assert_eq!(
            parse_grant("GRANT APPLICATION_PASSWORD_ADMIN,AUDIT_ADMIN ON *.* TO `root`@`%`"),
            privileges(
                &["APPLICATION_PASSWORD_ADMIN", "AUDIT_ADMIN"],
                Level::Global,
                user("root", "%"),
                false
            )
        );
        assert_eq!(
            parse_grant("GRANT PROXY ON `app`@`%` TO `ops`@`localhost`"),
            Line::Proxy {
                target: app(),
                grantee: user("ops", "localhost"),
                grantable: false,
            }
        );
    }

    #[test]
    fn mariadb_grants_read_hostless_roles_and_skip_the_default_role() {
        assert_eq!(
            parse_grant("GRANT `shop_write` TO `app`@`%`"),
            Line::Roles {
                roles: vec![role("shop_write")],
                grantee: user("app", "%"),
            }
        );
        assert_eq!(
            parse_grant("SET DEFAULT ROLE `shop_write` FOR `app`@`%`"),
            Line::Skipped
        );
        assert_eq!(
            parse_grant("GRANT `shop_read` TO `shop_write`"),
            Line::Roles {
                roles: vec![role("shop_read")],
                grantee: role("shop_write"),
            }
        );
        assert_eq!(
            parse_grant("GRANT SELECT ON `shop`.* TO `shop_read`"),
            privileges(&["SELECT"], database("shop"), role("shop_read"), false)
        );
        assert_eq!(
            parse_grant("GRANT `r` TO `u`@`%` WITH ADMIN OPTION"),
            Line::Roles {
                roles: vec![role("r")],
                grantee: user("u", "%"),
            }
        );
        assert_eq!(
            parse_grant(&format!(
                "GRANT ALL PRIVILEGES ON *.* TO `root`@`%` IDENTIFIED BY PASSWORD '{HASH}' \
                 WITH GRANT OPTION"
            )),
            privileges(&["ALL PRIVILEGES"], Level::Global, user("root", "%"), true)
        );
        assert_eq!(
            parse_grant("GRANT PROXY ON ``@`%` TO `root`@`%` WITH GRANT OPTION"),
            Line::Proxy {
                target: user("", "%"),
                grantee: user("root", "%"),
                grantable: true,
            }
        );
    }

    #[test]
    fn the_parser_reads_multi_word_privileges_and_resource_tails() {
        assert_eq!(
            parse_grant(
                "GRANT CREATE TEMPORARY TABLES, LOCK TABLES, REPLICATION SLAVE, \
                 REPLICATION CLIENT, CREATE ROUTINE, ALTER ROUTINE, CREATE VIEW, SHOW VIEW, \
                 CREATE USER, SHOW DATABASES, EVENT, TRIGGER ON *.* TO 'u'@'h' \
                 REQUIRE SSL WITH GRANT OPTION MAX_QUERIES_PER_HOUR 10"
            ),
            privileges(
                &[
                    "CREATE TEMPORARY TABLES",
                    "LOCK TABLES",
                    "REPLICATION SLAVE",
                    "REPLICATION CLIENT",
                    "CREATE ROUTINE",
                    "ALTER ROUTINE",
                    "CREATE VIEW",
                    "SHOW VIEW",
                    "CREATE USER",
                    "SHOW DATABASES",
                    "EVENT",
                    "TRIGGER",
                ],
                Level::Global,
                user("u", "h"),
                true
            )
        );
        assert_eq!(
            parse_grant("GRANT USAGE ON *.* TO 'u'@'h' WITH MAX_USER_CONNECTIONS 5"),
            privileges(&[], Level::Global, user("u", "h"), false)
        );
        assert_eq!(
            parse_grant("GRANT ALL ON FUNCTION `d`.`f` TO 'u'@'h'"),
            privileges(
                &["ALL PRIVILEGES"],
                Level::Routine {
                    database: "d".into(),
                    name: "f".into(),
                    kind: "function",
                },
                user("u", "h"),
                false
            )
        );
        assert_eq!(
            parse_grant("GRANT USAGE ON *.* TO 'u'@'h' IDENTIFIED VIA ed25519 USING 'secret-ish'"),
            privileges(&[], Level::Global, user("u", "h"), false)
        );
    }

    #[test]
    fn quotes_and_backticks_keep_their_doubled_and_escaped_characters() {
        assert_eq!(
            parse_grant("GRANT SELECT ON `we``ird`.* TO 'o''neil'@'h\\\\x'"),
            privileges(
                &["SELECT"],
                database("we`ird"),
                user("o'neil", "h\\x"),
                false
            )
        );
    }

    #[test]
    fn a_line_the_parser_cannot_read_is_kept_short_and_without_a_hash() {
        let odd_revoke = parse_grant("REVOKE SELECT ON `shop`.`orders` FROM `app`@`%`");
        assert_eq!(
            odd_revoke,
            Line::Unread("REVOKE SELECT ON `shop`.`orders` FROM `app`@`%`".into())
        );
        let hashed = parse_grant(&format!(
            "GRANT SELECT ON TO 'u'@'h' IDENTIFIED BY PASSWORD '{HASH}'"
        ));
        assert_eq!(hashed, Line::Unread("GRANT SELECT ON TO 'u'@'h'".into()));
        let long = parse_grant(&format!("GRANT {} ON", "X".repeat(200)));
        assert!(matches!(long, Line::Unread(text) if text.chars().count() == UNREAD_CHARS));
        for odd in [
            "",
            "GRANT",
            "GRANT `open",
            "GRANT SELECT ON *.* TO",
            "GRANT ( ON *.* TO a",
        ] {
            assert!(matches!(parse_grant(odd), Line::Unread(_)), "{odd}");
        }
    }

    /// MySQL with `partial_revokes` on lists a global privilege taken off one
    /// database as a `REVOKE` line of its own.
    #[test]
    fn a_partial_revoke_reads_as_what_it_takes_off_which_database() {
        assert_eq!(
            parse_grant("REVOKE SELECT, INSERT ON `mysql`.* FROM `app`@`%`"),
            Line::Revoke {
                privileges: vec![privilege("SELECT"), privilege("INSERT")],
                database: "mysql".into(),
                grantee: user("app", "%"),
            }
        );
        assert!(matches!(
            parse_grant("REVOKE SELECT ON *.* FROM `app`@`%`"),
            Line::Unread(_)
        ));
    }

    /// A global grant reaches every database but the ones a partial revoke
    /// takes it off; `ALL PRIVILEGES` there is what is left of it.
    #[test]
    fn access_takes_a_partial_revoke_off_its_database_alone() {
        let app = || user("app", "%");
        let held: Held = vec![
            (
                None,
                privileges(&["SELECT", "INSERT"], Level::Global, app(), false),
            ),
            (
                None,
                Line::Revoke {
                    privileges: vec![privilege("SELECT")],
                    database: "mysql".into(),
                    grantee: app(),
                },
            ),
        ];
        let listed = ["mysql".to_string(), "shop".into()];
        let access = assemble_access(&held, &listed, false);
        let rights = |name: &str| {
            let database = access.databases.iter().find(|d| d.name == name).unwrap();
            (database.rights.iter())
                .map(|right| right.privilege.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(rights("mysql"), ["INSERT"]);
        assert_eq!(rights("shop"), ["SELECT", "INSERT"]);
        assert!(access.server.iter().all(|line| !line.starts_with("unread")));

        let held: Held = vec![
            (
                None,
                privileges(&["ALL PRIVILEGES"], Level::Global, app(), false),
            ),
            (
                None,
                Line::Revoke {
                    privileges: vec![privilege("SELECT")],
                    database: "mysql".into(),
                    grantee: app(),
                },
            ),
        ];
        let access = assemble_access(&held, &listed, false);
        let mysql = rights_of(&access, "mysql");
        assert!(
            !mysql.contains(&"SELECT".to_string()) && !mysql.contains(&"ALL PRIVILEGES".into())
        );
        assert!(mysql.contains(&"INSERT".to_string()) && mysql.contains(&"DELETE".into()));
        assert_eq!(rights_of(&access, "shop"), ["ALL PRIVILEGES"]);

        let reach = assemble_reach(&held, &listed, &HashMap::new());
        assert!(
            reach
                .predefined
                .contains(&"SELECT revoked on mysql.*".to_string()),
            "{:?}",
            reach.predefined
        );
    }

    fn rights_of(access: &RoleAccess, name: &str) -> Vec<String> {
        let database = access.databases.iter().find(|d| d.name == name).unwrap();
        (database.rights.iter())
            .map(|right| right.privilege.clone())
            .collect()
    }

    /// With `partial_revokes` on, MySQL reads `_` and `%` in a schema-level
    /// grant as themselves: `sales_1.*` is not a pattern for `salesA1`.
    #[test]
    fn a_literal_schema_name_reaches_only_itself() {
        assert_eq!(literal("sales_1"), r"sales\_1");
        assert_eq!(literal(r"a\_b%"), r"a\_b\%");
        assert_eq!(database_source(&literal("sales_1"), "salesA1"), None);
        assert_eq!(
            database_source(&literal("sales_1"), "sales_1"),
            Some(Source::Direct)
        );
        let held = literal_levels(vec![(
            None,
            privileges(&["SELECT"], database("sales_1"), user("app", "%"), false),
        )]);
        let listed = ["sales_1".to_string(), "salesA1".into()];
        assert_eq!(reached_databases(&held, &listed), ["sales_1"]);
    }

    #[test]
    fn a_users_restrictions_say_what_is_revoked_where() {
        let restrictions = r#"[{"Database": "mysql", "Privileges": ["SELECT", "insert"]},
            {"Database": "shop", "Privileges": ["DELETE"]}]"#;
        assert_eq!(revoked_on(restrictions, "mysql"), ["SELECT", "INSERT"]);
        assert_eq!(revoked_on(restrictions, "shop"), ["DELETE"]);
        assert!(revoked_on(restrictions, "other").is_empty());
        assert!(revoked_on("", "mysql").is_empty(), "no restrictions column");
    }

    #[test]
    fn an_account_splits_at_its_last_at_and_quotes_for_show_grants() {
        assert_eq!(Account::parse("odd@name@%"), user("odd@name", "%"));
        assert_eq!(Account::parse("shop_read"), role("shop_read"));
        assert_eq!(user("odd@name", "%").display(), "odd@name@%");
        assert_eq!(user("odd@name", "%").quoted(), "'odd@name'@'%'");
        assert_eq!(user("o'neil", r"a\b").quoted(), r"'o''neil'@'a\\b'");
        assert_eq!(role("shop_read").quoted(), "'shop_read'");
        assert_eq!(Account::of(Engine::MariaDb, "r", ""), role("r"));
        assert_eq!(Account::of(Engine::MySql, "r", ""), user("r", ""));
    }

    #[test]
    fn like_patterns_match_as_mysql_reads_them() {
        assert!(is_pattern(r"db1\_%"));
        assert!(is_pattern("my_db"));
        assert!(!is_pattern(r"my\_db"));
        assert!(like_matches(r"db1\_%", "db1_sales"));
        assert!(!like_matches(r"db1\_%", "db1xsales"));
        assert!(like_matches("db_", "dbx"));
        assert!(!like_matches("db_", "db"));
        assert!(like_matches("%a%b%", "xxaxxbxx"));
        assert!(!like_matches("%a%b", "xxbxxa"));
        assert!(!like_matches("Shop", "shop"));
        assert_eq!(database_source(r"my\_db", "my_db"), Some(Source::Direct));
        assert_eq!(database_source("my_db", "my_db"), Some(Source::Direct));
        assert_eq!(
            database_source("my_db", "myxdb"),
            Some(Source::Pattern("my_db.*".into()))
        );
        assert_eq!(database_source("shop", "shops"), None);
    }

    #[test]
    fn the_account_columns_follow_the_engine_and_version() {
        for old in [(5, 6, 21), (5, 6, 51)] {
            let sql = accounts_sql(Engine::MySql, old);
            assert!(sql.contains("u.Password <> ''"), "{sql}");
            assert!(sql.contains("u.password_expired = 'Y'"), "{sql}");
            assert!(!sql.contains("account_locked"), "{sql}");
        }
        assert!(!accounts_sql(Engine::MySql, (5, 6, 5)).contains("password_expired"));
        for modern in [(5, 7, 44), (8, 0, 46)] {
            let sql = accounts_sql(Engine::MySql, modern);
            assert!(sql.contains("u.account_locked = 'Y'"), "{sql}");
            assert!(sql.contains("password_lifetime"), "{sql}");
            assert!(!sql.contains("Password <>"), "{sql}");
        }
        let old = accounts_sql(Engine::MariaDb, (10, 3, 39));
        assert!(old.contains("u.is_role = 'Y'"), "{old}");
        assert!(!old.contains("global_priv"), "{old}");
        assert!(!old.contains("password_expired"), "{old}");
        for modern in [(10, 4, 0), (11, 4, 2), (13, 0, 2)] {
            let sql = accounts_sql(Engine::MariaDb, modern);
            assert!(sql.contains("LEFT JOIN mysql.global_priv"), "{sql}");
            assert!(sql.contains("account_locked"), "{sql}");
            assert!(sql.contains("u.default_role"), "{sql}");
        }
        // Never the hash itself, on any server.
        for (engine, version) in [
            (Engine::MySql, (5, 6, 51)),
            (Engine::MySql, (8, 0, 46)),
            (Engine::MariaDb, (13, 0, 2)),
        ] {
            let sql = accounts_sql(engine, version);
            assert!(!sql.contains("u.authentication_string AS"), "{sql}");
            assert!(!sql.contains("u.Password AS"), "{sql}");
        }
        assert!(has_roles(Engine::MySql, (8, 0, 0)));
        assert!(!has_roles(Engine::MySql, (5, 7, 44)));
        assert!(has_roles(Engine::MariaDb, (10, 3, 0)));
    }

    /// `app` on MySQL 8, with a role and a global, a pattern and a column.
    fn held() -> Held {
        vec![
            (
                None,
                privileges(
                    &["SELECT", "PROCESS"],
                    Level::Global,
                    user("app", "%"),
                    false,
                ),
            ),
            (
                None,
                privileges(&["INSERT"], database(r"db1\_%"), user("app", "%"), false),
            ),
            (
                None,
                Line::Privileges {
                    privileges: vec![Privilege {
                        name: "UPDATE".into(),
                        columns: vec!["total".into(), "note".into()],
                    }],
                    level: table("shop", "orders"),
                    grantee: user("app", "%"),
                    grantable: false,
                },
            ),
            (
                None,
                privileges(
                    &["EXECUTE"],
                    Level::Routine {
                        database: "shop".into(),
                        name: "restock".into(),
                        kind: "procedure",
                    },
                    user("app", "%"),
                    false,
                ),
            ),
            (
                None,
                privileges(&["DELETE"], database("gone_%"), user("app", "%"), false),
            ),
            (
                None,
                Line::Proxy {
                    target: user("ops", "localhost"),
                    grantee: user("app", "%"),
                    grantable: false,
                },
            ),
            (
                None,
                Line::Roles {
                    roles: vec![user("shop_write", "%")],
                    grantee: user("app", "%"),
                },
            ),
            (
                Some("shop_write@%".into()),
                privileges(&["INSERT"], database("shop"), user("shop_write", "%"), true),
            ),
            (
                None,
                Line::Unread("REVOKE SELECT ON `mysql`.* FROM `app`@`%`".into()),
            ),
        ]
    }

    #[test]
    fn access_puts_global_data_privileges_in_every_database_and_the_rest_on_the_server() {
        let listed = ["db1_sales".to_string(), "other".into(), "shop".into()];
        let access = assemble_access(&held(), &listed, false);
        assert_eq!(
            access.server,
            [
                "PROCESS",
                "PROXY ON 'ops'@'localhost'",
                "unread: REVOKE SELECT ON `mysql`.* FROM `app`@`%`",
            ]
        );
        let names: Vec<&str> = access.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["db1_sales", "other", "shop", "gone_%"]);
        let rights = |name: &str| {
            let database = access.databases.iter().find(|d| d.name == name).unwrap();
            (database.rights.iter())
                .map(|right| (right.privilege.clone(), right.sources.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rights("db1_sales"),
            [
                ("SELECT".to_string(), vec![Source::Global]),
                (
                    "INSERT".to_string(),
                    vec![Source::Pattern(r"db1\_%.*".into())]
                ),
            ]
        );
        assert_eq!(
            rights("other"),
            [("SELECT".to_string(), vec![Source::Global])]
        );
        assert_eq!(
            rights("shop"),
            [
                ("SELECT".to_string(), vec![Source::Global]),
                (
                    "INSERT".to_string(),
                    vec![Source::Via("shop_write@%".into())]
                ),
                (
                    "GRANT OPTION".to_string(),
                    vec![Source::Via("shop_write@%".into())]
                ),
            ]
        );
        assert_eq!(
            rights("gone_%"),
            [(
                "DELETE".to_string(),
                vec![Source::Pattern("gone_%.*".into())]
            )]
        );
        let shop = access.databases.iter().find(|d| d.name == "shop").unwrap();
        let inside = shop.inside.as_ref().unwrap();
        assert!(inside.flat && inside.schemas.is_empty());
        let held: Vec<(&str, &str)> = (inside.held.iter())
            .map(|grant| (grant.relation.as_str(), grant.privilege.as_str()))
            .collect();
        assert_eq!(
            held,
            [
                ("orders (note, total)", "UPDATE"),
                ("restock (procedure)", "EXECUTE")
            ]
        );
        assert!(
            inside
                .held
                .iter()
                .all(|grant| grant.schema == "shop" && grant.usable)
        );
        assert!(access.databases.iter().all(|d| d.allows_connections));
    }

    #[test]
    fn nothing_held_lists_no_database_and_no_hash_survives() {
        let line = parse_grant(&format!(
            "GRANT USAGE ON *.* TO 'nopriv'@'%' IDENTIFIED BY PASSWORD '{HASH}'"
        ));
        let access = assemble_access(&vec![(None, line)], &["shop".into()], false);
        assert!(access.databases.is_empty() && access.server.is_empty());
        let unread = parse_grant(&format!("GRANT ??? TO x IDENTIFIED BY PASSWORD '{HASH}'"));
        assert_eq!(unread, Line::Unread("GRANT ??? TO x".into()));
        let access = assemble_access(&vec![(None, unread)], &[], false);
        assert!(!format!("{access:?}").contains("*DB14"), "{access:?}");
    }

    #[test]
    fn a_superuser_holds_everything_on_star_star_with_grant_option() {
        let all = |grantable| {
            vec![(
                None,
                privileges(
                    &["ALL PRIVILEGES"],
                    Level::Global,
                    user("root", "%"),
                    grantable,
                ),
            )]
        };
        assert!(holds_everything(&all(true)));
        assert!(!holds_everything(&all(false)));
        let listed = vec![(
            None,
            privileges(
                &SUPERUSER_PRIVILEGES,
                Level::Global,
                user("root", "%"),
                true,
            ),
        )];
        assert!(holds_everything(&listed));
        let access = assemble_access(&all(true), &["shop".into()], true);
        assert_eq!(access.server, ["ALL PRIVILEGES (grantable)"]);
        assert!(access.superuser);
    }

    #[test]
    fn reach_counts_tables_per_database_and_says_what_star_star_gives() {
        let listed = ["db1_sales".to_string(), "other".into(), "shop".into()];
        let held = held();
        assert_eq!(reached_databases(&held, &listed), ["db1_sales", "shop"]);
        let tables = HashMap::from([("db1_sales".to_string(), 4), ("shop".to_string(), 7)]);
        let reach = assemble_reach(&held, &listed, &tables);
        let entries: Vec<(&str, &str, u64, Vec<String>)> = (reach.entries.iter())
            .map(|e| {
                (
                    e.schema.as_str(),
                    e.privilege.as_str(),
                    e.relations,
                    e.through.clone(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("db1_sales", "INSERT", 4, vec![]),
                ("shop", "INSERT", 7, vec!["shop_write@%".to_string()]),
                ("shop", "UPDATE (columns)", 1, vec![]),
            ]
        );
        assert_eq!(
            reach.predefined,
            [
                "SELECT, PROCESS on *.*",
                "PROXY ON 'ops'@'localhost'",
                "unread: REVOKE SELECT ON `mysql`.* FROM `app`@`%`",
            ]
        );
        assert!(reach.database.is_empty());
        assert_eq!(
            tables_sql(Engine::MySql, &["a'b".into()]),
            "SELECT TABLE_SCHEMA AS name, COUNT(*) AS tables FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA IN ('a''b') GROUP BY TABLE_SCHEMA"
        );
    }

    fn result(columns: &[&str], rows: &[&[Option<&str>]]) -> QueryResult {
        QueryResult {
            columns: (columns.iter())
                .map(|name| super::super::Column {
                    name: name.to_string(),
                    data_type: None,
                })
                .collect(),
            rows: rows
                .iter()
                .map(|row| Row::new(row.iter().copied()).unwrap())
                .collect(),
            ..QueryResult::default()
        }
    }

    #[test]
    fn mysql_8_memberships_are_inherited_only_when_default_or_all_activate() {
        let edges = result(
            &["role_user", "role_host", "member_user", "member_host"],
            &[
                &[Some("shop_write"), Some("%"), Some("app"), Some("%")],
                &[
                    Some("shop_read"),
                    Some("%"),
                    Some("reader"),
                    Some("10.0.0.%"),
                ],
            ],
        );
        let defaults = result(
            &["member_user", "member_host", "role_user", "role_host"],
            &[&[Some("app"), Some("%"), Some("shop_write"), Some("%")]],
        );
        let (memberships, not_inherited) = assemble_edges(&edges, &defaults, false).unwrap();
        assert_eq!(
            memberships,
            [
                ("app@%".to_string(), "shop_write@%".to_string()),
                ("reader@10.0.0.%".to_string(), "shop_read@%".to_string()),
            ]
        );
        assert_eq!(
            not_inherited,
            [("reader@10.0.0.%".to_string(), "shop_read@%".to_string())]
        );
        assert!(
            assemble_edges(&edges, &defaults, true)
                .unwrap()
                .1
                .is_empty()
        );
    }

    #[test]
    fn mariadb_memberships_follow_the_default_role_and_roles_always_inherit() {
        let mapping = result(
            &["member_user", "member_host", "role_user"],
            &[
                &[Some("app"), Some("%"), Some("shop_write")],
                &[Some("reader"), Some("10.0.0.%"), Some("shop_read")],
                &[Some("shop_write"), Some(""), Some("shop_read")],
            ],
        );
        let defaults = HashMap::from([(user("app", "%"), "shop_write".to_string())]);
        let (memberships, not_inherited) = assemble_mapping(&mapping, &defaults).unwrap();
        assert_eq!(memberships.len(), 3);
        assert!(memberships.contains(&("shop_write".into(), "shop_read".into())));
        assert_eq!(
            not_inherited,
            [("reader@10.0.0.%".to_string(), "shop_read".to_string())]
        );
    }

    #[test]
    fn accounts_read_locks_roles_limits_and_never_a_password() {
        let accounts = result(
            &[
                "user",
                "host",
                "superuser",
                "has_password",
                "locked",
                "is_role",
                "expired",
                "expires",
                "default_role",
                "connection_limit",
            ],
            &[
                &[
                    Some("app"),
                    Some("%"),
                    Some("n"),
                    Some("y"),
                    Some("n"),
                    Some("n"),
                    Some("n"),
                    Some("2027-01-31"),
                    Some("shop_write"),
                    Some("5"),
                ],
                &[
                    Some("locked"),
                    Some("%"),
                    Some("n"),
                    Some("y"),
                    Some("y"),
                    Some("n"),
                    Some("y"),
                    None,
                    None,
                    Some("0"),
                ],
                &[
                    Some("shop_read"),
                    Some(""),
                    Some("n"),
                    Some("n"),
                    Some("n"),
                    Some("y"),
                    Some("n"),
                    None,
                    None,
                    Some("0"),
                ],
            ],
        );
        let (roles, defaults) = assemble_accounts(Engine::MariaDb, &accounts).unwrap();
        let app = &roles[0];
        assert_eq!(app.name, "app@%");
        assert!(app.login && !app.superuser);
        assert_eq!(app.password_expires.as_deref(), Some("2027-01-31"));
        assert_eq!(app.connection_limit, Some(5));
        assert_eq!(
            defaults.get(&user("app", "%")).map(String::as_str),
            Some("shop_write")
        );
        assert!(roles[1].deactivated() && roles[1].expired);
        assert_eq!(roles[1].connection_limit, None);
        assert_eq!(roles[2].name, "shop_read");
        assert!(!roles[2].login && !roles[2].deactivated());

        let all = Roles {
            roles,
            memberships: vec![("app@%".into(), "shop_write".into())],
            ..Roles::default()
        };
        let sessions = result(&["name", "sessions"], &[&[Some("app"), Some("3")]]);
        let reports = assemble_reports(&all, &sessions, false).unwrap();
        assert_eq!(reports.expired, ["locked@%"]);
        assert_eq!(reports.unused_roles, ["shop_read"]);
        assert_eq!(reports.sessions, [("app".to_string(), 3)]);
        assert_eq!(reports.idle, None);
        assert_eq!(
            reports.public_grants,
            Some(Vec::new()),
            "MySQL has no PUBLIC"
        );
        // MariaDB 10.11's PUBLIC is not read: unknown, never zero.
        let reports = assemble_reports(&all, &sessions, true).unwrap();
        assert_eq!(reports.public_grants, None);
    }

    #[test]
    fn object_grants_say_where_each_grant_is_written() {
        let mut privilege_columns: Vec<&str> = vec!["user", "host"];
        privilege_columns.extend(TABLE_PRIVILEGES.iter().map(|(_, column)| *column));
        privilege_columns.push("Grant_priv");
        let flags = |user: &str, host: &str, on: &[&str], grant: bool| {
            let mut row: Vec<Option<String>> = vec![Some(user.into()), Some(host.into())];
            row.extend(
                TABLE_PRIVILEGES
                    .iter()
                    .map(|(name, _)| Some(if on.contains(name) { "Y" } else { "N" }.to_string())),
            );
            row.push(Some(if grant { "Y" } else { "N" }.into()));
            row
        };
        let build = |columns: &[&str], rows: Vec<Vec<Option<String>>>| QueryResult {
            columns: (columns.iter())
                .map(|name| super::super::Column {
                    name: name.to_string(),
                    data_type: None,
                })
                .collect(),
            rows: rows.into_iter().map(|row| Row::new(row).unwrap()).collect(),
            ..QueryResult::default()
        };
        let global = build(
            &privilege_columns,
            vec![flags("ops", "localhost", &["SELECT"], false)],
        );
        let mut db_columns = vec!["db"];
        db_columns.extend(&privilege_columns);
        let with_db = |db: &str, row: Vec<Option<String>>| {
            let mut full = vec![Some(db.to_string())];
            full.extend(row);
            full
        };
        let databases = build(
            &db_columns,
            vec![
                with_db("shop", flags("app", "%", &["SELECT", "INSERT"], false)),
                with_db("sh_p", flags("reader", "10.0.0.%", &["SELECT"], true)),
                with_db("other", flags("x", "%", &["SELECT"], false)),
            ],
        );
        let table = result(
            &["user", "host", "privileges"],
            &[
                &[Some("reader"), Some("10.0.0.%"), Some("Select,Grant")],
                &[Some("app"), Some("%"), Some("")],
            ],
        );
        let columns = result(
            &["user", "host", "name", "privileges"],
            &[
                &[Some("app"), Some("%"), Some("total"), Some("Update")],
                &[Some("app"), Some("%"), Some("note"), Some("Update")],
            ],
        );
        let grants = assemble_object_grants(
            Engine::MySql,
            "shop",
            false,
            &global,
            &databases,
            &table,
            &columns,
            Roles::default(),
        )
        .unwrap();
        assert!(!grants.public && !grants.sequence && grants.owner.is_empty());
        assert_eq!(grants.privileges.len(), 12);
        let listed: Vec<(String, String, bool, Option<String>)> = (grants.grants.iter())
            .map(|g| {
                (
                    g.grantee.clone().unwrap(),
                    g.privilege.clone(),
                    g.grantable,
                    g.on.clone(),
                )
            })
            .collect();
        let row = |grantee: &str, privilege: &str, grantable, on: Option<&str>| {
            (
                grantee.to_string(),
                privilege.to_string(),
                grantable,
                on.map(str::to_string),
            )
        };
        assert_eq!(
            listed,
            [
                row("app@%", "SELECT", false, Some("shop.*")),
                row("app@%", "INSERT", false, Some("shop.*")),
                row("app@%", "UPDATE (note, total)", false, None),
                row("ops@localhost", "SELECT", false, Some("*.*")),
                row("reader@10.0.0.%", "SELECT", true, None),
                row("reader@10.0.0.%", "SELECT", true, Some("sh_p.*")),
            ]
        );
    }

    /// A connection to the server `variable` names, plaintext as the test
    /// servers speak, or `None` when it is not set. These servers are seeded
    /// by hand (`docs/agents/dev-databases.md`); CI runs every ignored test and
    /// has none of them, so an unset variable skips rather than fails.
    fn live(variable: &str, engine: Engine) -> Option<super::super::mysql::Connection> {
        let Ok(url) = std::env::var(variable) else {
            eprintln!("skipped: {variable} is not set");
            return None;
        };
        let config = super::super::mysql::config_from_url(&url)
            .unwrap_or_else(|_| panic!("{variable} should parse"));
        let connection = super::super::mysql::Connection::open(
            &super::super::ServerConfig {
                sslmode: super::super::SslMode::Disable,
                ..config
            },
            engine,
        )
        .unwrap_or_else(|error| panic!("{variable}: the connection should open: {error:?}"));
        Some(connection)
    }

    fn listed<'a>(roles: &'a Roles, name: &str) -> &'a Role {
        roles
            .role(name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    }

    /// The seeded 5.6 server: no roles, `reader`'s database pattern, a
    /// proxy and `odd@name`'s `@`.
    #[test]
    #[ignore = "requires a MySQL 5.6 configured through dbdelve_MYSQL56_URL"]
    fn live_users_and_grants_on_mysql_56() {
        let Some(connection) = live("dbdelve_MYSQL56_URL", Engine::MySql) else {
            return;
        };
        let roles = connection.roles().unwrap();
        assert_eq!(roles.denied, None);
        for name in [
            "app@%",
            "reader@10.0.0.%",
            "ops@localhost",
            "nopriv@%",
            "odd@name@%",
        ] {
            let role = listed(&roles, name);
            assert!(role.login && role.has_password == Some(true), "{name}");
        }
        assert!(roles.memberships.is_empty());
        assert!(!format!("{roles:?}").contains("*DB14"));

        let app = connection.role_access("app@%").unwrap();
        let shop = app.databases.iter().find(|d| d.name == "shop").unwrap();
        let privileges: Vec<&str> = shop.rights.iter().map(|r| r.privilege.as_str()).collect();
        assert_eq!(privileges, ["SELECT", "INSERT"]);
        let inside = shop.inside.as_ref().unwrap();
        let relations: Vec<&str> = inside.held.iter().map(|g| g.relation.as_str()).collect();
        assert_eq!(relations, ["orders (note, total)", "restock (procedure)"]);
        assert!(!format!("{app:?}").contains("*DB14"));

        // The seed holds no database the pattern matches, so it is a
        // section of its own, named as written.
        let reader = connection.role_access("reader@10.0.0.%").unwrap();
        let pattern = (reader.databases.iter())
            .find(|d| d.name == r"db1\_%")
            .unwrap_or_else(|| panic!("{reader:?}"));
        assert_eq!(
            pattern.rights[0].sources,
            [Source::Pattern(r"db1\_%.*".into())]
        );
        let ops = connection.role_access("ops@localhost").unwrap();
        assert!(
            ops.server.contains(&"PROCESS".to_string()),
            "{:?}",
            ops.server
        );
        assert!(
            ops.server.contains(&"PROXY ON 'app'@'%'".to_string()),
            "{:?}",
            ops.server
        );
        let odd = connection.role_access("odd@name@%").unwrap();
        assert!(odd.databases.iter().any(|d| d.name == "shop"));

        let reach = connection.role_reach("app@%").unwrap();
        assert!(
            reach
                .entries
                .iter()
                .any(|e| e.schema == "shop" && e.privilege == "SELECT")
        );
        let grants = connection.object_grants("shop", "orders").unwrap();
        assert!(grants.grants.iter().any(
            |g| g.grantee.as_deref() == Some("app@%") && g.privilege == "UPDATE (note, total)"
        ));
        assert!(
            grants
                .grants
                .iter()
                .any(|g| g.grantee.as_deref() == Some("app@%")
                    && g.privilege == "SELECT"
                    && g.on.as_deref() == Some("shop.*"))
        );
        connection.role_reports(&roles).unwrap();
    }

    /// MySQL 8 and MariaDB, seeded alike: `app` holds `shop_write` by
    /// default, `reader` holds `shop_read` but not by default, and
    /// `shop_write` holds `shop_read`.
    fn live_roles(variable: &str, engine: Engine, role: &dyn Fn(&str) -> String) {
        let Some(connection) = live(variable, engine) else {
            return;
        };
        let roles = connection.roles().unwrap();
        assert_eq!(roles.denied, None);
        let (read, write) = (role("shop_read"), role("shop_write"));
        for name in ["app@%", "reader@10.0.0.%", "ops@localhost", "nopriv@%"] {
            assert!(listed(&roles, name).login, "{name}");
        }
        let locked = listed(&roles, "locked@%");
        assert!(locked.deactivated() && locked.expired, "{locked:?}");
        for name in [&read, &write] {
            let role = listed(&roles, name);
            assert!(!role.login && !role.deactivated(), "{role:?}");
        }
        assert!(roles.member_of("app@%").contains(&write.as_str()));
        assert!(roles.member_of(&write).contains(&read.as_str()));
        assert!(roles.inherits("app@%", &write));
        assert!(!roles.inherits("reader@10.0.0.%", &read));

        let app = connection.role_access("app@%").unwrap();
        let shop = app.databases.iter().find(|d| d.name == "shop").unwrap();
        let select = shop
            .rights
            .iter()
            .find(|r| r.privilege == "SELECT")
            .unwrap();
        assert_eq!(select.sources, [Source::Via(read.clone())]);
        let inside = shop.inside.as_ref().unwrap();
        assert!(
            inside
                .held
                .iter()
                .any(|g| g.relation == "orders (note, total)" && g.through.is_none())
        );
        assert!(inside.held.iter().any(|g| g.relation == "orders"
            && g.privilege == "INSERT"
            && g.through.as_deref() == Some(write.as_str())));
        assert!(!format!("{app:?}").contains("*DB14"));

        let ops = connection.role_access("ops@localhost").unwrap();
        assert!(
            ops.server.contains(&"PROXY ON 'app'@'%'".to_string()),
            "{:?}",
            ops.server
        );

        let reach = connection.role_reach("app@%").unwrap();
        assert!(
            reach.entries.iter().any(|e| e.schema == "shop"
                && e.privilege == "SELECT"
                && e.through == [read.clone()])
        );
        let grants = connection.object_grants("shop", "orders").unwrap();
        assert!(
            grants
                .grants
                .iter()
                .any(|g| g.grantee.as_deref() == Some(write.as_str())
                    && g.privilege == "INSERT"
                    && g.on.is_none())
        );
        assert!(
            grants
                .grants
                .iter()
                .any(|g| g.grantee.as_deref() == Some(read.as_str())
                    && g.on.as_deref() == Some("shop.*"))
        );
        let reports = connection.role_reports(&roles).unwrap();
        assert!(
            reports.expired.contains(&"locked@%".to_string()),
            "{reports:?}"
        );
        assert!(!reports.unused_roles.contains(&read), "{reports:?}");
        assert!(connection.role_grants("app@%").is_err());
    }

    #[test]
    #[ignore = "requires a seeded MySQL 8 configured through dbdelve_MYSQL_ROLES_URL"]
    fn live_users_and_grants_on_mysql_8() {
        live_roles("dbdelve_MYSQL_ROLES_URL", Engine::MySql, &|name| {
            format!("{name}@%")
        });
    }

    #[test]
    #[ignore = "requires a seeded MariaDB configured through dbdelve_MARIADB_ROLES_URL"]
    fn live_users_and_grants_on_mariadb() {
        live_roles("dbdelve_MARIADB_ROLES_URL", Engine::MariaDb, &|name| {
            name.to_string()
        });
    }

    /// A login with no privilege on `mysql.user` is told so, not failed.
    #[test]
    #[ignore = "requires logins with no privileges through dbdelve_MYSQL56_NOPRIV_URL, \
                dbdelve_MYSQL_NOPRIV_URL and dbdelve_MARIADB_NOPRIV_URL"]
    fn live_a_login_without_the_privilege_is_denied_calmly() {
        for (variable, engine) in [
            ("dbdelve_MYSQL56_NOPRIV_URL", Engine::MySql),
            ("dbdelve_MYSQL_NOPRIV_URL", Engine::MySql),
            ("dbdelve_MARIADB_NOPRIV_URL", Engine::MariaDb),
        ] {
            let Some(connection) = live(variable, engine) else {
                continue;
            };
            let roles = connection.roles().unwrap();
            assert_eq!(roles.denied.as_deref(), Some(DENIED), "{variable}");
            assert!(roles.roles.is_empty(), "{variable}");
            let refused = connection.object_grants("shop", "orders").unwrap_err();
            assert_eq!(refused.message, GRANTS_DENIED, "{variable}");
        }
    }
}
