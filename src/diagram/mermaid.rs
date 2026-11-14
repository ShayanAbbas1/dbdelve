//! The diagram as Mermaid `erDiagram` text, the one format that pastes into
//! a GitHub issue or README and draws there. It lists what the diagram
//! shows: the tables of the view on screen, every column or only the keys as
//! Keys only says, and a relationship per foreign key, labelled with its
//! columns.
//!
//! Mermaid's grammar is narrow. An entity name is a bare word or quoted
//! (with no `"`, `%` or backslash inside); an attribute's type and name are
//! bare words of letters, digits, `_`, `-`, brackets and parentheses. So
//! names and types are spelled into those, and an attribute whose spelling
//! lost something carries its real name or type in its comment.

use std::collections::HashSet;
use std::fmt::Write;

use crate::db::{GraphColumn, SchemaGraph};

use super::geometry::{ChildMark, Detail, ParentMark};
use super::layout::{Layout, Subject};
use super::svg::card_name;

pub fn mermaid(graph: &SchemaGraph, layout: &Layout) -> String {
    let detail = layout.detail(1.0);
    let mut taken = HashSet::new();
    let names: Vec<String> = (layout.cards.iter())
        .map(|card| unique(entity(&card_name(graph, card.subject)), &mut taken))
        .collect();
    let mut out = String::from("erDiagram\n");
    for (card, name) in layout.cards.iter().zip(&names) {
        let table = match card.subject {
            Subject::Table(table) => table,
            Subject::Stub(_) => {
                _ = writeln!(out, "    %% In another schema: its columns are not read.");
                _ = writeln!(out, "    {name}");
                continue;
            }
        };
        let columns = &graph.tables[table as usize].columns;
        let shown: Vec<&GraphColumn> = match detail {
            Detail::Columns => columns.iter().collect(),
            _ => (card.key_rows.iter())
                .map(|&column| &columns[column as usize])
                .collect(),
        };
        if shown.is_empty() {
            _ = writeln!(out, "    {name}");
            continue;
        }
        _ = writeln!(out, "    {name} {{");
        for column in shown {
            let type_label = &*graph.types[column.type_label as usize];
            _ = write!(out, "        {} {}", word(type_label), word(&column.name));
            let keys: Vec<&str> = [
                (column.primary, "PK"),
                (column.foreign, "FK"),
                (column.unique && !column.primary, "UK"),
            ]
            .into_iter()
            .filter_map(|(holds, key)| holds.then_some(key))
            .collect();
            if !keys.is_empty() {
                _ = write!(out, " {}", keys.join(", "));
            }
            // A type whose spaces became underscores reads the same; one
            // that lost more (a comma, a quote) is worth its real spelling.
            let respelled: Vec<&str> = [
                (&*column.name, word(&column.name) != *column.name),
                (type_label, word(type_label) != type_label.replace(' ', "_")),
            ]
            .into_iter()
            .filter_map(|(text, respelled)| respelled.then_some(text))
            .collect();
            if !respelled.is_empty() {
                _ = write!(out, " \"{}\"", quoted(&respelled.join(" ")));
            }
            out.push('\n');
        }
        out.push_str("    }\n");
    }

    for edge in &layout.edges {
        let key = &graph.keys[edge.key as usize];
        let parent = match edge.parent_mark {
            ParentMark::ExactlyOne => "||",
            ParentMark::ZeroOrOne => "|o",
        };
        let child = match edge.child_mark {
            ChildMark::Many => "o{",
            ChildMark::One => "o|",
        };
        let columns = &graph.tables[key.table as usize].columns;
        // A key whose columns are all in the child's primary key makes the
        // child's rows depend on the parent's: Mermaid's identifying line.
        let line = match key
            .columns
            .iter()
            .all(|&column| columns[column as usize].primary)
        {
            true => "--",
            false => "..",
        };
        // The key's columns rather than its constraint name, which is
        // generated noise on most schemas and a bare number on SQLite.
        let label = (key.columns.iter())
            .map(|&column| &*columns[column as usize].name)
            .collect::<Vec<_>>()
            .join(", ");
        _ = writeln!(
            out,
            "    {} {parent}{line}{child} {} : \"{}\"",
            names[edge.parent as usize],
            names[edge.child as usize],
            quoted(&label),
        );
    }
    out
}

/// An entity's name: bare where Mermaid reads it bare, else quoted, with
/// what a quoted name cannot hold spelled otherwise.
fn entity(name: &str) -> String {
    let mut chars = name.chars();
    let bare = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if bare {
        return name.to_string();
    }
    let inside: String = name
        .chars()
        .map(|c| match c {
            '"' => '\'',
            '\\' => '/',
            '%' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    match inside.trim() {
        "" => "\"_\"".to_string(),
        inside => format!("\"{inside}\""),
    }
}

/// `name`, or with a number after it where an earlier entity took it, as two
/// names can become one once spelled for Mermaid.
fn unique(name: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.clone()) {
        return name;
    }
    let (open, close) = match name.strip_suffix('"') {
        Some(quoted) => (quoted.to_string(), "\""),
        None => (name.clone(), ""),
    };
    (2..)
        .map(|n| format!("{open}_{n}{close}"))
        .find(|candidate| taken.insert(candidate.clone()))
        .expect("some number is free")
}

/// A type or column name as one of Mermaid's attribute words.
fn word(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '[' | ']' | '(' | ')') => c,
            _ => '_',
        })
        .collect();
    if !out
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
    {
        out.insert(0, '_');
    }
    out
}

/// Text inside a Mermaid string, which ends at the first `"`. Mermaid
/// takes `%%` anywhere for a comment or a directive's bounds before it
/// reads a string, so the second `%` of a pair is spelled `_`: a lone one
/// is kept, as the real name holds it.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        out.push(match c {
            '"' => '\'',
            '%' if out.ends_with('%') => '_',
            c if c.is_control() => ' ',
            c => c,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{GraphKey, KeyTarget};
    use crate::diagram::layout::{neighbourhood, schema};
    use crate::diagram::tests::graph;

    #[test]
    fn tables_list_their_columns_and_keys_draw_their_cardinality() {
        let mut graph = graph(3, &[(1, 0), (2, 1)]);
        // t2's key into t1 is nullable, and t1's into t0 is unique: 1:1.
        graph.tables[2].columns[1].nullable = true;
        graph.tables[1].columns[1].unique = true;
        let layout = schema(&graph, false);
        let text = mermaid(&graph, &layout);
        assert_eq!(
            text,
            "erDiagram
    t0 {
        bigint id PK
    }
    t1 {
        bigint id PK
        bigint t0_id FK, UK
    }
    t2 {
        bigint id PK
        bigint t1_id FK
    }
    t0 ||..o| t1 : \"t0_id\"
    t1 |o..o{ t2 : \"t1_id\"
"
        );
    }

    #[test]
    fn awkward_names_and_types_are_spelled_for_mermaid_and_kept_in_comments() {
        let mut graph = graph(2, &[(1, 0)]);
        graph.tables[0].name = "order items".into();
        graph.tables[1].name = "say \"hi\" 100%\\".into();
        graph.tables[1].columns[1].name = "1st col".into();
        graph.types = vec!["character varying(255)".into(), "numeric(10,2)".into()];
        graph.tables[0].columns[0].type_label = 1;
        let text = mermaid(&graph, &schema(&graph, false));
        assert!(text.contains("    \"order items\" {\n"), "{text}");
        assert!(text.contains("    \"say 'hi' 100_/\" {\n"), "{text}");
        assert!(
            text.contains("        character_varying(255) _1st_col FK \"1st col\"\n"),
            "{text}"
        );
        assert!(
            text.contains("        character_varying(255) id PK\n"),
            "{text}"
        );
        assert!(
            text.contains("        numeric(10_2) id PK \"numeric(10,2)\"\n"),
            "{text}"
        );
        assert!(text.contains("    \"order items\" ||..o{ \"say 'hi' 100_/\" : \"1st col\"\n"));
    }

    /// Mermaid lifts `%%{...}%%` out of the text as a directive before it
    /// parses, quotes or not, so a name could reconfigure the renderer.
    #[test]
    fn a_name_cannot_smuggle_a_directive_into_a_comment_or_a_label() {
        let payload = "x%%{init:{'theme':'dark'}}%%";
        let mut graph = graph(2, &[(1, 0)]);
        graph.tables[1].columns[1].name = payload.into();
        let text = mermaid(&graph, &schema(&graph, false));
        assert!(!text.contains("%%"), "{text}");
        assert!(
            text.contains(" \"x%_{init:{'theme':'dark'}}%_\"\n"),
            "{text}"
        );
    }

    #[test]
    fn names_that_spell_alike_stay_apart() {
        let mut graph = graph(2, &[]);
        graph.tables[0].name = "a\"b".into();
        graph.tables[1].name = "a'b".into();
        let text = mermaid(&graph, &schema(&graph, false));
        assert!(
            text.contains("    \"a'b\" {\n") && text.contains("    \"a'b_2\" {\n"),
            "{text}"
        );
    }

    #[test]
    fn a_key_into_another_schema_is_an_entity_of_its_qualified_name_and_keys_only_lists_keys() {
        let mut graph = graph(1, &[]);
        graph.tables[0].columns.push(GraphColumn {
            name: "owner".into(),
            type_label: 0,
            nullable: false,
            primary: false,
            foreign: true,
            unique: false,
        });
        graph.tables[0].columns.push(GraphColumn {
            name: "payload".into(),
            type_label: 0,
            nullable: true,
            primary: false,
            foreign: false,
            unique: false,
        });
        graph.keys.push(GraphKey {
            name: "".into(),
            table: 0,
            columns: vec![1],
            referenced: KeyTarget::Elsewhere {
                schema: "auth".into(),
                table: "users".into(),
            },
        });
        let text = mermaid(&graph, &schema(&graph, true));
        assert!(
            text.contains(
                "    %% In another schema: its columns are not read.\n    \"auth.users\"\n"
            )
        );
        assert!(!text.contains("payload"), "keys only: {text}");
        assert!(
            text.contains("    \"auth.users\" ||..o{ t0 : \"owner\"\n"),
            "{text}"
        );
    }

    #[test]
    fn a_neighbourhood_lists_its_tables_only_and_a_self_reference_is_one_relationship() {
        let graph = graph(4, &[(1, 0), (2, 1), (3, 3)]);
        let text = mermaid(&graph, &neighbourhood(&graph, 1, false));
        assert!(!text.contains("t3"), "{text}");
        assert_eq!(text.matches(" : ").count(), 2);
        let looped = mermaid(&graph, &neighbourhood(&graph, 3, false));
        assert!(
            looped.contains("    t3 ||..o{ t3 : \"t3_id\"\n"),
            "{looped}"
        );
    }
}
