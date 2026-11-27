//! Per-tool and per-class policy for agent calls.
//!
//! Policy sits around [`crate::sql::agent_gate_on`], never inside it. It can
//! refuse a call the gate allowed or hold it for a click, and can let a
//! gate-approved write run without a click when the owner has opted in, but it
//! never turns a gate `Refuse` into anything else and never widens what a
//! connection's mode allows.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::sql::{AgentVerdict, Mode, Refusal};

use super::tools::Tool;

/// What DBDelve does when an agent calls a tool or statement class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyLevel {
    Deny,
    Ask,
    Auto,
}

impl PolicyLevel {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Auto => "auto",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Deny => "Deny",
            Self::Ask => "Ask",
            Self::Auto => "Auto",
        }
    }

    pub(crate) fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Every row in the policy table: the 9 schema/plan tools and the 3 classes of
/// `execute_sql` statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum PolicyRow {
    ListConnections = 0,
    SearchSchema = 1,
    ListSchemas = 2,
    ListTables = 3,
    DescribeTable = 4,
    ListRelationships = 5,
    ListRoutines = 6,
    DescribeRoutine = 7,
    ExplainQuery = 8,
    ExecuteSqlReads = 9,
    ExecuteSqlWrites = 10,
    ExecuteSqlSensitiveReads = 11,
}

impl PolicyRow {
    pub(crate) const SCHEMA_TOOLS: [Self; 9] = [
        Self::ListConnections,
        Self::SearchSchema,
        Self::ListSchemas,
        Self::ListTables,
        Self::DescribeTable,
        Self::ListRelationships,
        Self::ListRoutines,
        Self::DescribeRoutine,
        Self::ExplainQuery,
    ];

    pub(crate) const SQL_CLASSES: [Self; 3] = [
        Self::ExecuteSqlReads,
        Self::ExecuteSqlWrites,
        Self::ExecuteSqlSensitiveReads,
    ];

    pub(crate) const ALL: [Self; 12] = [
        Self::ListConnections,
        Self::SearchSchema,
        Self::ListSchemas,
        Self::ListTables,
        Self::DescribeTable,
        Self::ListRelationships,
        Self::ListRoutines,
        Self::DescribeRoutine,
        Self::ExplainQuery,
        Self::ExecuteSqlReads,
        Self::ExecuteSqlWrites,
        Self::ExecuteSqlSensitiveReads,
    ];

    /// The row for a non-`execute_sql` tool, or `None` for `execute_sql`,
    /// whose class is decided from the gate's verdict.
    pub(crate) fn from_tool(tool: Tool) -> Option<Self> {
        match tool {
            Tool::ListConnections => Some(Self::ListConnections),
            Tool::SearchSchema => Some(Self::SearchSchema),
            Tool::ListSchemas => Some(Self::ListSchemas),
            Tool::ListTables => Some(Self::ListTables),
            Tool::DescribeTable => Some(Self::DescribeTable),
            Tool::ListRelationships => Some(Self::ListRelationships),
            Tool::ListRoutines => Some(Self::ListRoutines),
            Tool::DescribeRoutine => Some(Self::DescribeRoutine),
            Tool::ExplainQuery => Some(Self::ExplainQuery),
            Tool::ExecuteSql => None,
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::ListConnections => "list_connections",
            Self::SearchSchema => "search_schema",
            Self::ListSchemas => "list_schemas",
            Self::ListTables => "list_tables",
            Self::DescribeTable => "describe_table",
            Self::ListRelationships => "list_relationships",
            Self::ListRoutines => "list_routines",
            Self::DescribeRoutine => "describe_routine",
            Self::ExplainQuery => "explain_query",
            Self::ExecuteSqlReads => "execute_sql_reads",
            Self::ExecuteSqlWrites => "execute_sql_writes",
            Self::ExecuteSqlSensitiveReads => "execute_sql_sensitive_reads",
        }
    }

    pub(crate) fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|row| row.slug() == slug)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ExecuteSqlReads => "Reads",
            Self::ExecuteSqlWrites => "Writes",
            Self::ExecuteSqlSensitiveReads => "Sensitive reads",
            other => other.slug(),
        }
    }

    pub(crate) fn default_level(self) -> PolicyLevel {
        match self {
            Self::ExecuteSqlWrites => PolicyLevel::Ask,
            Self::ExecuteSqlSensitiveReads => PolicyLevel::Deny,
            _ => PolicyLevel::Auto,
        }
    }

    /// Sensitive reads offer Deny and Ask only, never Auto.
    pub(crate) fn allows(self, level: PolicyLevel) -> bool {
        match self {
            Self::ExecuteSqlSensitiveReads => matches!(level, PolicyLevel::Deny | PolicyLevel::Ask),
            _ => true,
        }
    }

    pub(crate) fn choices(self) -> &'static [PolicyLevel] {
        match self {
            Self::ExecuteSqlSensitiveReads => &[PolicyLevel::Deny, PolicyLevel::Ask],
            _ => &[PolicyLevel::Deny, PolicyLevel::Ask, PolicyLevel::Auto],
        }
    }
}

/// The saved and live policy across every tool and `execute_sql` class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentPolicy {
    levels: [PolicyLevel; 12],
}

impl Default for AgentPolicy {
    fn default() -> Self {
        let mut levels = [PolicyLevel::Deny; 12];
        for row in PolicyRow::ALL {
            levels[row as usize] = row.default_level();
        }
        Self { levels }
    }
}

impl AgentPolicy {
    pub(crate) fn get(&self, row: PolicyRow) -> PolicyLevel {
        let level = self.levels[row as usize];
        if row.allows(level) {
            level
        } else {
            row.default_level()
        }
    }

    /// Set `row` to `level` when `row` admits it; returns whether the value
    /// was accepted.
    pub(crate) fn set(&mut self, row: PolicyRow, level: PolicyLevel) -> bool {
        if !row.allows(level) {
            return false;
        }
        self.levels[row as usize] = level;
        true
    }

    /// The policy level for a non-`execute_sql` tool.
    pub(crate) fn tool_level(&self, tool: Tool) -> Option<PolicyLevel> {
        PolicyRow::from_tool(tool).map(|row| self.get(row))
    }

    /// Whether `list_tools` advertises `tool`: a schema tool set to Deny is
    /// omitted; `execute_sql` stays listed so its classes are enforced per
    /// statement.
    pub(crate) fn tool_listed(&self, tool: Tool) -> bool {
        match PolicyRow::from_tool(tool) {
            Some(row) => self.get(row) != PolicyLevel::Deny,
            None => true,
        }
    }

    /// How many tools `list_tools` currently advertises.
    pub(crate) fn listed_tool_count(&self) -> usize {
        Tool::ALL
            .into_iter()
            .filter(|tool| self.tool_listed(*tool))
            .count()
    }

    /// Decode from `[settings.agent_policy]`. Any missing, unknown or
    /// disallowed entry falls back to that row's default, never more
    /// permissive.
    pub(crate) fn from_stored(stored: Option<&BTreeMap<String, String>>) -> Self {
        let mut policy = Self::default();
        let Some(stored) = stored else {
            return policy;
        };
        for (key, value) in stored {
            if let Some(row) = PolicyRow::from_slug(key)
                && let Some(level) = PolicyLevel::from_slug(value)
                && row.allows(level)
            {
                policy.levels[row as usize] = level;
            }
        }
        policy
    }

    /// Encode only the rows that differ from their defaults, or `None` when
    /// every row is at its default.
    pub(crate) fn to_stored(&self) -> Option<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        for row in PolicyRow::ALL {
            let level = self.get(row);
            if level != row.default_level() {
                map.insert(row.slug().to_string(), level.slug().to_string());
            }
        }
        (!map.is_empty()).then_some(map)
    }

    /// One-line summary of the `execute_sql` classes for the mark's hover
    /// preview.
    pub(crate) fn summary(&self) -> String {
        format!(
            "Reads {}, writes {}, sensitive reads {}.",
            self.get(PolicyRow::ExecuteSqlReads).label(),
            self.get(PolicyRow::ExecuteSqlWrites).label(),
            self.get(PolicyRow::ExecuteSqlSensitiveReads).label(),
        )
    }
}

/// What the bridge does with an `execute_sql` call once [`agent_gate_on`] has
/// judged it and [`AgentPolicy`] has been applied around that verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SqlPolicyDecision {
    /// The gate itself refused the statement; policy never changes this.
    RefuseByGate(Refusal),
    /// The policy row for this statement's class is Deny.
    RefuseByPolicy(PolicyRow),
    /// The policy row for this statement's class is Ask; wait for the human.
    Ask { row: PolicyRow, is_write: bool },
    /// The gate said Run and reads are Auto.
    RunRead,
    /// The gate approved a write and writes are Auto.
    RunWrite,
}

/// Apply `policy` around `verdict` for an `execute_sql` call.
///
/// A gate `Refuse` is returned unchanged regardless of policy. Sensitive reads
/// (`Approve` with a `ReadOnly` verdict) can never return `RunRead` or
/// `RunWrite`.
pub(crate) fn resolve_sql(policy: &AgentPolicy, verdict: &AgentVerdict) -> SqlPolicyDecision {
    match verdict {
        AgentVerdict::Refuse(reason) => SqlPolicyDecision::RefuseByGate(*reason),
        AgentVerdict::Run => {
            let row = PolicyRow::ExecuteSqlReads;
            match policy.get(row) {
                PolicyLevel::Deny => SqlPolicyDecision::RefuseByPolicy(row),
                PolicyLevel::Ask => SqlPolicyDecision::Ask {
                    row,
                    is_write: false,
                },
                PolicyLevel::Auto => SqlPolicyDecision::RunRead,
            }
        }
        AgentVerdict::Approve(held, _) if held.mode == Mode::ReadOnly => {
            let row = PolicyRow::ExecuteSqlSensitiveReads;
            match policy.get(row) {
                PolicyLevel::Ask => SqlPolicyDecision::Ask {
                    row,
                    is_write: false,
                },
                // Sensitive reads never offer Auto; fail closed to Deny.
                PolicyLevel::Deny | PolicyLevel::Auto => SqlPolicyDecision::RefuseByPolicy(row),
            }
        }
        AgentVerdict::Approve(_, _) => {
            let row = PolicyRow::ExecuteSqlWrites;
            match policy.get(row) {
                PolicyLevel::Deny => SqlPolicyDecision::RefuseByPolicy(row),
                PolicyLevel::Ask => SqlPolicyDecision::Ask {
                    row,
                    is_write: true,
                },
                PolicyLevel::Auto => SqlPolicyDecision::RunWrite,
            }
        }
    }
}

/// Read `[settings.agent_policy]` directly from a `profiles.toml` file without
/// touching `crate::db` or the keychain, so the MCP shim can filter
/// `list_tools` on its own. Missing or unreadable files fall back to the
/// default policy.
pub(crate) fn read_saved_policy(path: &Path) -> AgentPolicy {
    #[derive(Deserialize)]
    struct File {
        settings: Option<Settings>,
    }
    #[derive(Deserialize)]
    struct Settings {
        agent_policy: Option<BTreeMap<String, String>>,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return AgentPolicy::default();
    };
    let Ok(parsed) = toml::from_str::<File>(&text) else {
        return AgentPolicy::default();
    };
    AgentPolicy::from_stored(
        parsed
            .settings
            .as_ref()
            .and_then(|settings| settings.agent_policy.as_ref()),
    )
}

/// The message returned to an agent when a schema tool is denied by policy.
pub(crate) fn tool_denied_text(tool: Tool) -> String {
    format!(
        "The `{}` tool is denied by DBDelve's AI Features policy.",
        tool.name()
    )
}

/// The message returned to an agent when an `execute_sql` class is denied by
/// policy.
pub(crate) fn sql_class_denied_text(row: PolicyRow) -> String {
    match row {
        PolicyRow::ExecuteSqlReads => {
            "Read statements are denied by DBDelve's AI Features policy.".to_string()
        }
        PolicyRow::ExecuteSqlWrites => {
            "Write statements are denied by DBDelve's AI Features policy.".to_string()
        }
        PolicyRow::ExecuteSqlSensitiveReads => {
            "Sensitive reads (files, URLs, or unlisted functions) are denied by DBDelve's AI Features policy.".to_string()
        }
        other => format!(
            "The `{}` tool is denied by DBDelve's AI Features policy.",
            other.slug()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::Engine, sql::agent_gate};

    #[test]
    fn defaults_are_schema_and_reads_auto_writes_ask_sensitive_deny() {
        let policy = AgentPolicy::default();
        for row in PolicyRow::SCHEMA_TOOLS {
            assert_eq!(policy.get(row), PolicyLevel::Auto, "{row:?}");
        }
        assert_eq!(policy.get(PolicyRow::ExecuteSqlReads), PolicyLevel::Auto);
        assert_eq!(policy.get(PolicyRow::ExecuteSqlWrites), PolicyLevel::Ask);
        assert_eq!(
            policy.get(PolicyRow::ExecuteSqlSensitiveReads),
            PolicyLevel::Deny
        );
        assert_eq!(policy.to_stored(), None);
        assert_eq!(policy.listed_tool_count(), Tool::ALL.len());
    }

    #[test]
    fn missing_unknown_and_disallowed_entries_fall_back_to_default_never_more_permissive() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "execute_sql_sensitive_reads".to_string(),
            "auto".to_string(),
        );
        raw.insert("execute_sql_writes".to_string(), "bogus".to_string());
        raw.insert("list_tables".to_string(), "allow_all".to_string());
        raw.insert("unknown_tool".to_string(), "auto".to_string());

        let policy = AgentPolicy::from_stored(Some(&raw));
        // Sensitive reads must NOT accept "auto"; it stays at its default Deny.
        assert_eq!(
            policy.get(PolicyRow::ExecuteSqlSensitiveReads),
            PolicyLevel::Deny
        );
        // Unknown values fall back to each row's default.
        assert_eq!(policy.get(PolicyRow::ExecuteSqlWrites), PolicyLevel::Ask);
        assert_eq!(policy.get(PolicyRow::ListTables), PolicyLevel::Auto);

        // `set` also refuses Auto on sensitive reads.
        let mut live = AgentPolicy::default();
        assert!(!live.set(PolicyRow::ExecuteSqlSensitiveReads, PolicyLevel::Auto));
        assert_eq!(
            live.get(PolicyRow::ExecuteSqlSensitiveReads),
            PolicyLevel::Deny
        );
        assert!(live.set(PolicyRow::ExecuteSqlSensitiveReads, PolicyLevel::Ask));
        assert_eq!(
            live.get(PolicyRow::ExecuteSqlSensitiveReads),
            PolicyLevel::Ask
        );
    }

    #[test]
    fn policy_never_turns_a_gate_refuse_into_anything_else() {
        let mut permissive = AgentPolicy::default();
        for row in PolicyRow::ALL {
            let _ = permissive.set(row, PolicyLevel::Auto);
        }
        let _ = permissive.set(PolicyRow::ExecuteSqlSensitiveReads, PolicyLevel::Ask);

        let refused_statements = [
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "DELETE FROM t WHERE id = 1",
            "ALTER TABLE t DROP COLUMN a",
            "SET search_path = public",
            "SELECT 1; SELECT 2",
            "",
        ];
        for sql in refused_statements {
            let verdict = agent_gate(Engine::Postgres, sql, Mode::Full);
            assert!(
                matches!(verdict, AgentVerdict::Refuse(_)),
                "expected gate Refuse for {sql:?}, got {verdict:?}"
            );
            assert!(
                matches!(
                    resolve_sql(&permissive, &verdict),
                    SqlPolicyDecision::RefuseByGate(_)
                ),
                "policy changed gate Refuse for {sql:?}"
            );
        }
    }

    #[test]
    fn auto_write_is_still_held_back_by_read_only_mode_and_named_refusals() {
        let mut policy = AgentPolicy::default();
        assert!(policy.set(PolicyRow::ExecuteSqlWrites, PolicyLevel::Auto));

        // Read-only mode still refuses writes at the gate.
        let ro_verdict = agent_gate(Engine::Postgres, "UPDATE t SET a = 1", Mode::ReadOnly);
        assert_eq!(
            resolve_sql(&policy, &ro_verdict),
            SqlPolicyDecision::RefuseByGate(Refusal::NeedsMode {
                needed: Mode::ReadWrite,
                held: Mode::ReadOnly,
            })
        );

        // Read-write mode allows an UPDATE, and Auto runs it.
        let rw_verdict = agent_gate(Engine::Postgres, "UPDATE t SET a = 1", Mode::ReadWrite);
        assert_eq!(
            resolve_sql(&policy, &rw_verdict),
            SqlPolicyDecision::RunWrite
        );

        // Named refusals still fail even in Read-write/Full with Writes = Auto.
        for (sql, expected) in [
            ("DELETE FROM t WHERE id = 1", Refusal::Delete),
            ("DROP TABLE t", Refusal::Drop),
            ("TRUNCATE TABLE t", Refusal::Truncate),
            ("ALTER TABLE t DROP COLUMN a", Refusal::Alter),
        ] {
            let verdict = agent_gate(Engine::Postgres, sql, Mode::ReadWrite);
            assert_eq!(
                resolve_sql(&policy, &verdict),
                SqlPolicyDecision::RefuseByGate(expected),
                "{sql}"
            );
        }
    }

    #[test]
    fn resolve_sql_maps_every_class_and_level() {
        let read_verdict = agent_gate(Engine::Postgres, "SELECT 1", Mode::ReadWrite);
        let write_verdict = agent_gate(
            Engine::Postgres,
            "INSERT INTO t VALUES (1)",
            Mode::ReadWrite,
        );
        let egress_verdict = agent_gate(
            Engine::Postgres,
            "SELECT pg_read_file('a')",
            Mode::ReadWrite,
        );
        let unlisted_verdict = agent_gate(Engine::Postgres, "SELECT my_udf(1)", Mode::ReadWrite);
        let duckdb_path_verdict =
            agent_gate(Engine::DuckDb, "SELECT * FROM 'a.csv'", Mode::ReadOnly);

        // Defaults: reads Auto, writes Ask, sensitive reads Deny.
        let defaults = AgentPolicy::default();
        assert_eq!(
            resolve_sql(&defaults, &read_verdict),
            SqlPolicyDecision::RunRead
        );
        assert_eq!(
            resolve_sql(&defaults, &write_verdict),
            SqlPolicyDecision::Ask {
                row: PolicyRow::ExecuteSqlWrites,
                is_write: true,
            }
        );
        for sensitive in [&egress_verdict, &unlisted_verdict, &duckdb_path_verdict] {
            assert_eq!(
                resolve_sql(&defaults, sensitive),
                SqlPolicyDecision::RefuseByPolicy(PolicyRow::ExecuteSqlSensitiveReads)
            );
        }

        // Flip every class.
        let mut flipped = AgentPolicy::default();
        flipped.set(PolicyRow::ExecuteSqlReads, PolicyLevel::Ask);
        flipped.set(PolicyRow::ExecuteSqlWrites, PolicyLevel::Deny);
        flipped.set(PolicyRow::ExecuteSqlSensitiveReads, PolicyLevel::Ask);
        assert_eq!(
            resolve_sql(&flipped, &read_verdict),
            SqlPolicyDecision::Ask {
                row: PolicyRow::ExecuteSqlReads,
                is_write: false,
            }
        );
        assert_eq!(
            resolve_sql(&flipped, &write_verdict),
            SqlPolicyDecision::RefuseByPolicy(PolicyRow::ExecuteSqlWrites)
        );
        for sensitive in [&egress_verdict, &unlisted_verdict, &duckdb_path_verdict] {
            assert_eq!(
                resolve_sql(&flipped, sensitive),
                SqlPolicyDecision::Ask {
                    row: PolicyRow::ExecuteSqlSensitiveReads,
                    is_write: false,
                }
            );
        }

        // Reads Deny.
        flipped.set(PolicyRow::ExecuteSqlReads, PolicyLevel::Deny);
        assert_eq!(
            resolve_sql(&flipped, &read_verdict),
            SqlPolicyDecision::RefuseByPolicy(PolicyRow::ExecuteSqlReads)
        );
    }

    #[test]
    fn stored_round_trip_and_list_tools_omits_denied_schema_tools() {
        let mut policy = AgentPolicy::default();
        policy.set(PolicyRow::ListTables, PolicyLevel::Deny);
        policy.set(PolicyRow::DescribeTable, PolicyLevel::Ask);
        policy.set(PolicyRow::ExecuteSqlWrites, PolicyLevel::Auto);
        policy.set(PolicyRow::ExecuteSqlSensitiveReads, PolicyLevel::Ask);

        let stored = policy.to_stored().expect("non-default policy serializes");
        assert_eq!(stored.len(), 4);
        assert_eq!(AgentPolicy::from_stored(Some(&stored)), policy);

        assert!(!policy.tool_listed(Tool::ListTables));
        assert!(policy.tool_listed(Tool::DescribeTable));
        assert!(policy.tool_listed(Tool::ExecuteSql));
        assert_eq!(policy.listed_tool_count(), Tool::ALL.len() - 1);
    }
}
