//! The diagram as Mermaid `erDiagram` text, the one format that pastes into
//! a GitHub issue or README and draws there. It lists what the diagram
//! shows: the tables of the view on screen, every column or only the keys as
//! Keys only says, and a relationship per foreign key, labelled with its
//! columns.
//!
//! Mermaid's grammar is narrow. An entity name is quoted here (with no
//! `"`, `%` or backslash inside); an attribute's type and name are
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

/// An entity's name as Mermaid identifies it, once its quotes are
/// stripped: what a quoted name cannot hold spelled otherwise, and never
/// empty.
fn entity(name: &str) -> String {
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
        "" => "_".to_string(),
        inside => undirected(inside.to_string()),
    }
}

/// `name` quoted, with a number after it where an earlier entity took it,
/// as two names can become one once spelled for Mermaid. Always quoted:
/// Mermaid's lexer tries its keywords before a bare name, case blind and
/// without a word boundary, so a bare `classes` reads as `class` and `es`.
fn unique(name: String, taken: &mut HashSet<String>) -> String {
    let name = match taken.insert(name.clone()) {
        true => name,
        false => (2..)
            .map(|n| format!("{name}_{n}"))
            .find(|candidate| taken.insert(candidate.clone()))
            .expect("some number is free"),
    };
    format!("\"{name}\"")
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
    // Mermaid reads PK, FK or UK as a key marker, case blind, wherever a
    // word boundary follows, before it tries an attribute word.
    let marker = out.get(..2).is_some_and(|start| {
        ["pk", "fk", "uk"].contains(&start.to_ascii_lowercase().as_str())
            && !(out[2..].chars().next())
                .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_')
    });
    if marker
        || !out
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
    undirected(out)
}

/// Mermaid takes a line holding `direction`, white space and `TB`, `BT`,
/// `RL` or `LR`, case blind and anywhere on it, for a direction statement
/// before it reads a string, so the space after such a `direction` is
/// spelled `_`.
fn undirected(text: String) -> String {
    // JavaScript's `\s`, which a byte-order mark is in and Unicode's white
    // space is not.
    let space = |c: &char| c.is_whitespace() || *c == '\u{feff}';
    let mut chars: Vec<char> = text.chars().collect();
    for at in 0..chars.len() {
        let word: String = chars[at..].iter().take(9).collect();
        if !word.eq_ignore_ascii_case("direction") {
            continue;
        }
        let gap = chars[at + 9..].iter().take_while(|c| space(c)).count();
        let way: String = chars[at + 9 + gap..].iter().take(2).collect();
        if gap > 0
            && ["tb", "bt", "rl", "lr"]
                .iter()
                .any(|d| way.eq_ignore_ascii_case(d))
        {
            chars[at + 9] = '_';
        }
    }
    chars.into_iter().collect()
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
    \"t0\" {
        bigint id PK
    }
    \"t1\" {
        bigint id PK
        bigint t0_id FK, UK
    }
    \"t2\" {
        bigint id PK
        bigint t1_id FK
    }
    \"t0\" ||..o| \"t1\" : \"t0_id\"
    \"t1\" |o..o{ \"t2\" : \"t1_id\"
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

    /// Mermaid's lexer reads any line with `direction LR` in it as a
    /// direction statement, the whole line, quotes or not.
    #[test]
    fn a_name_holding_a_direction_does_not_take_its_line() {
        let mut graph = graph(2, &[(1, 0)]);
        graph.tables[0].name = "direction LR".into();
        graph.tables[1].name = "Direction\u{a0}\ttb_and_more".into();
        graph.tables[1].columns[1].name = "sort direction rl".into();
        let text = mermaid(&graph, &schema(&graph, false));
        assert!(text.contains("    \"direction_LR\" {\n"), "{text}");
        assert!(
            text.contains("    \"Direction_ tb_and_more\" {\n"),
            "{text}"
        );
        assert!(text.contains(" : \"sort direction_rl\"\n"), "{text}");
        let lexed_as_direction = |line: &str| {
            let line = line.to_ascii_lowercase();
            line.match_indices("direction").any(|(at, _)| {
                let rest = &line[at + "direction".len()..];
                let way = rest.trim_start_matches(|c: char| c.is_whitespace());
                way.len() < rest.len()
                    && ["tb", "bt", "rl", "lr"].iter().any(|d| way.starts_with(d))
            })
        };
        assert!(!text.lines().any(lexed_as_direction), "{text}");
    }

    /// Mermaid strips an entity's quotes, so `_` and `"_"` are one entity.
    #[test]
    fn names_that_spell_alike_once_unquoted_stay_apart() {
        let mut graph = graph(3, &[]);
        graph.tables[0].name = "_".into();
        graph.tables[1].name = "%".into();
        graph.tables[2].name = "__2".into();
        let text = mermaid(&graph, &schema(&graph, false));
        let entities: Vec<&str> = (text.lines())
            .filter_map(|line| line.strip_suffix(" {"))
            .map(|line| line.trim().trim_matches('"'))
            .collect();
        let distinct: HashSet<&str> = entities.iter().copied().collect();
        assert_eq!(distinct.len(), 3, "{text}");
    }

    /// Mermaid's lexer tries its keywords before a bare name, case blind
    /// and without a word boundary, so `classes` reads as `class` + `es`.
    #[test]
    fn a_name_that_starts_like_a_keyword_is_quoted() {
        let names = [
            "style",
            "One",
            "classes",
            "tokens",
            "many_more",
            "end",
            "1",
            "u-turn",
        ];
        let mut graph = graph(names.len(), &[]);
        for (table, name) in graph.tables.iter_mut().zip(names) {
            table.name = name.into();
        }
        let text = mermaid(&graph, &schema(&graph, false));
        for name in names {
            assert!(
                text.contains(&format!("    \"{name}\" {{\n")),
                "{name}: {text}"
            );
        }
    }

    /// Inside a block, Mermaid reads PK, FK and UK as key markers before an
    /// attribute word, case blind, wherever a word boundary follows them.
    #[test]
    fn a_column_or_type_spelled_like_a_key_marker_is_prefixed() {
        let mut graph = graph(1, &[]);
        let names = ["PK", "fk", "uk-ref", "Pk(1)", "pk_id", "fkey"];
        graph.tables[0].columns = (names.iter())
            .map(|name| GraphColumn {
                name: (*name).into(),
                type_label: 0,
                nullable: false,
                primary: false,
                foreign: false,
                unique: false,
            })
            .collect();
        graph.types = vec!["UK".into()];
        let text = mermaid(&graph, &schema(&graph, false));
        for line in [
            "        _UK _PK \"PK UK\"\n",
            "        _UK _fk \"fk UK\"\n",
            "        _UK _uk-ref \"uk-ref UK\"\n",
            "        _UK _Pk(1) \"Pk(1) UK\"\n",
            "        _UK pk_id \"UK\"\n",
            "        _UK fkey \"UK\"\n",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
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
            text.contains("    \"auth.users\" ||..o{ \"t0\" : \"owner\"\n"),
            "{text}"
        );
    }

    #[test]
    fn a_neighbourhood_lists_its_tables_only_and_a_self_reference_is_one_relationship() {
        let graph = graph(4, &[(1, 0), (2, 1), (3, 3)]);
        let text = mermaid(&graph, &neighbourhood(&graph, 1, false));
        assert!(!text.contains("\"t3\""), "{text}");
        assert_eq!(text.matches(" : ").count(), 2);
        let looped = mermaid(&graph, &neighbourhood(&graph, 3, false));
        assert!(
            looped.contains("    \"t3\" ||..o{ \"t3\" : \"t3_id\"\n"),
            "{looped}"
        );
    }
}
