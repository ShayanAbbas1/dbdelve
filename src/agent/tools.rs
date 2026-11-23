//! The agent's tool list. Static: it never varies with what is visible or what
//! state the app is in, because a list that changed under the agent would be a
//! tool the agent had approved turning into another. Only results vary.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A tool the agent can call, spelled on the wire as its MCP name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tool {
    ListConnections,
    /// Finds schemas, relations and routines by name across a connection's
    /// whole catalog.
    SearchSchema,
    /// Lists a connection's schemas from its catalog.
    ListSchemas,
    /// Lists one schema's relations from the connection's catalog.
    ListTables,
    /// Describes one relation the catalog holds: columns, keys and indexes.
    DescribeTable,
    /// Lists one schema's foreign keys, every relation's at once.
    ListRelationships,
    /// Lists one schema's functions and procedures by signature.
    ListRoutines,
    /// Gives the definition of every routine of one name in one schema.
    DescribeRoutine,
    /// Runs one statement: a read at once, anything else once the user has
    /// approved it in the window.
    ExecuteSql,
    /// Asks the database for the plan of one statement, without running it
    /// unless asked to measure it.
    ExplainQuery,
}

impl Tool {
    /// Every tool, in the order the agent is shown them.
    pub(crate) const ALL: [Tool; 10] = [
        Tool::ListConnections,
        Tool::SearchSchema,
        Tool::ListSchemas,
        Tool::ListTables,
        Tool::DescribeTable,
        Tool::ListRelationships,
        Tool::ListRoutines,
        Tool::DescribeRoutine,
        Tool::ExecuteSql,
        Tool::ExplainQuery,
    ];

    /// The name the agent calls it by: the spelling serde writes for it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tool::ListConnections => "list_connections",
            Tool::SearchSchema => "search_schema",
            Tool::ListSchemas => "list_schemas",
            Tool::ListTables => "list_tables",
            Tool::DescribeTable => "describe_table",
            Tool::ListRelationships => "list_relationships",
            Tool::ListRoutines => "list_routines",
            Tool::DescribeRoutine => "describe_routine",
            Tool::ExecuteSql => "execute_sql",
            Tool::ExplainQuery => "explain_query",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Tool::ListConnections => {
                "Lists the DBDelve connections the user has made visible to agents, by name and engine."
            }
            Tool::SearchSchema => {
                "Finds schemas, tables, views, functions and procedures of a named connection whose names match `query`, best match first. The match is fuzzy: the query's characters in order, not necessarily together. Column names are not searched; describe_table gives a relation's columns."
            }
            Tool::ListSchemas => "Lists the schemas of a named connection, in name order.",
            Tool::ListTables => {
                "Lists the tables and views of one schema of a named connection, with each one's kind, its parent when it is a partition, its estimated row count and its size on disk in bytes, either null where the database keeps none."
            }
            Tool::DescribeTable => {
                "Describes one table or view of a named connection: its columns with type, nullability and default, its primary key, its foreign keys and its indexes."
            }
            Tool::ListRelationships => {
                "Lists the foreign keys of one schema of a named connection: for each, the table and columns that hold it and the schema, table and columns it references, columns paired by position."
            }
            Tool::ListRoutines => {
                "Lists the functions and procedures of one schema of a named connection, with each one's kind, arguments, result type and language. Overloads are listed one per row."
            }
            Tool::DescribeRoutine => {
                "Gives the definition of a function or procedure of a named connection, by schema and name: every overload of that name, each with its arguments, result type, language and body."
            }
            Tool::ExecuteSql => {
                "Runs one statement on a named connection exactly as written; one statement per call. A read answers at once with the first rows, header first. A statement that changes data, reads a file or URL, or calls a function DBDelve does not list waits until the user approves or denies it in the DBDelve window, and the call returns when they answer. The status is one of executed, rejected, expired, refused_by_gate, failed or app_not_running; app_not_running means DBDelve is closed or its agent access is off, and the user has to open it or turn access back on before a retry can succeed. DELETE, DROP, TRUNCATE, most ALTER and more than one statement per call are refused."
            }
            Tool::ExplainQuery => {
                "Returns the plan the database would use for one statement of a named connection, findings first and then the plan tree. With `analyze` true it runs the statement to measure it, which only a read may do; without it nothing is run. A statement that needs the user's approval is not run by this tool."
            }
        }
    }

    /// The JSON Schema of the arguments the tool takes. None takes more
    /// than it names, so an argument it does not know is refused up front
    /// rather than ignored.
    pub(crate) fn input_schema(self) -> Map<String, Value> {
        let mut schema = Map::new();
        schema.insert("type".to_string(), json!("object"));
        let properties = match self {
            Tool::ListConnections => json!({}),
            Tool::SearchSchema => {
                schema.insert("required".to_string(), json!(["connection", "query"]));
                json!({ "connection": { "type": "string" }, "query": { "type": "string" } })
            }
            Tool::ListSchemas => {
                schema.insert("required".to_string(), json!(["connection"]));
                json!({ "connection": { "type": "string" } })
            }
            Tool::ListTables | Tool::ListRelationships | Tool::ListRoutines => {
                schema.insert("required".to_string(), json!(["connection", "schema"]));
                json!({ "connection": { "type": "string" }, "schema": { "type": "string" } })
            }
            Tool::DescribeTable => {
                schema.insert(
                    "required".to_string(),
                    json!(["connection", "schema", "table"]),
                );
                json!({
                    "connection": { "type": "string" },
                    "schema": { "type": "string" },
                    "table": { "type": "string" },
                })
            }
            Tool::DescribeRoutine => {
                schema.insert(
                    "required".to_string(),
                    json!(["connection", "schema", "routine"]),
                );
                json!({
                    "connection": { "type": "string" },
                    "schema": { "type": "string" },
                    "routine": { "type": "string" },
                })
            }
            Tool::ExecuteSql => {
                schema.insert("required".to_string(), json!(["connection", "sql"]));
                json!({ "connection": { "type": "string" }, "sql": { "type": "string" } })
            }
            Tool::ExplainQuery => {
                schema.insert("required".to_string(), json!(["connection", "sql"]));
                json!({
                    "connection": { "type": "string" },
                    "sql": { "type": "string" },
                    "analyze": { "type": "boolean" },
                })
            }
        };
        schema.insert("properties".to_string(), properties);
        schema.insert("additionalProperties".to_string(), json!(false));
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_spelled_the_same_in_its_name_and_on_the_wire() {
        for tool in Tool::ALL {
            assert_eq!(serde_json::to_value(tool).unwrap(), json!(tool.name()));
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
        assert_eq!(Tool::from_name("List_Connections"), None);
    }

    #[test]
    fn execute_sql_says_a_change_waits_for_the_user() {
        let text = Tool::ExecuteSql.description();
        for word in [
            "approve",
            "DBDelve window",
            "executed",
            "rejected",
            "expired",
            "refused_by_gate",
            "app_not_running",
        ] {
            assert!(text.contains(word), "{word}: {text}");
        }
        assert!(!text.contains("is not run"), "{text}");
    }

    #[test]
    fn no_tool_takes_an_argument_it_does_not_name() {
        for tool in Tool::ALL {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
        }
        assert_eq!(Tool::ALL.len(), 10);
        assert_eq!(
            Tool::ListSchemas.input_schema()["required"],
            json!(["connection"])
        );
        let schema = Tool::ListTables.input_schema();
        assert_eq!(schema["required"], json!(["connection", "schema"]));
        assert_eq!(schema["properties"]["schema"], json!({"type": "string"}));
        for tool in [Tool::ListRelationships, Tool::ListRoutines] {
            assert_eq!(
                tool.input_schema()["required"],
                json!(["connection", "schema"])
            );
        }
        let schema = Tool::SearchSchema.input_schema();
        assert_eq!(schema["required"], json!(["connection", "query"]));
        let schema = Tool::DescribeRoutine.input_schema();
        assert_eq!(
            schema["required"],
            json!(["connection", "schema", "routine"])
        );
        let schema = Tool::DescribeTable.input_schema();
        assert_eq!(schema["required"], json!(["connection", "schema", "table"]));
        assert_eq!(schema["properties"]["table"], json!({"type": "string"}));
        let schema = Tool::ExecuteSql.input_schema();
        assert_eq!(schema["required"], json!(["connection", "sql"]));
        assert_eq!(
            schema["properties"]["connection"],
            json!({"type": "string"})
        );
        assert_eq!(schema["properties"]["sql"], json!({"type": "string"}));
        // The statement is the only text an agent supplies to an approval.
        let mut named: Vec<_> = schema["properties"]
            .as_object()
            .expect("the properties are an object")
            .keys()
            .map(String::as_str)
            .collect();
        named.sort_unstable();
        assert_eq!(named, ["connection", "sql"]);
        let schema = Tool::ExplainQuery.input_schema();
        assert_eq!(schema["required"], json!(["connection", "sql"]));
        assert_eq!(schema["properties"]["sql"], json!({"type": "string"}));
        assert_eq!(schema["properties"]["analyze"], json!({"type": "boolean"}));
    }
}
