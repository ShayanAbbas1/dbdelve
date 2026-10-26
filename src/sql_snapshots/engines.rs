use super::{check, fragment, visible};
use crate::{
    connection_form,
    db::{ColumnDefinition, Engine, NamedDefinition, RelationKind, Structure},
    filter::{self, Operator},
};

fn answer(out: &mut String, engine: Engine, case: &str, value: impl std::fmt::Debug) {
    fragment(out, engine, case, Some(&format!("{value:?}")));
}

fn column(default: Option<&str>) -> ColumnDefinition {
    ColumnDefinition {
        name: "c".into(),
        data_type: "int".into(),
        nullable: true,
        default: default.map(Into::into),
    }
}

fn named(definition: &str) -> NamedDefinition {
    NamedDefinition {
        name: "n".into(),
        definition: definition.into(),
    }
}

fn structures() -> [(&'static str, Structure); 4] {
    [
        ("no key", Structure::default()),
        (
            "primary key constraint",
            Structure {
                constraints: vec![named("PRIMARY KEY (id)")],
                ..Structure::default()
            },
        ),
        (
            "unique constraint",
            Structure {
                constraints: vec![named("UNIQUE (email)")],
                ..Structure::default()
            },
        ),
        (
            "lower-case unique index",
            Structure {
                indexes: vec![named("create unique index u on t (email)")],
                ..Structure::default()
            },
        ),
    ]
}

const KINDS: [RelationKind; 5] = [
    RelationKind::Table,
    RelationKind::PartitionedTable,
    RelationKind::View,
    RelationKind::MaterializedView,
    RelationKind::ForeignTable,
];

#[test]
fn engine_answers_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        fragment(&mut out, engine, "label", Some(engine.label()));
        fragment(&mut out, engine, "as_str", Some(engine.as_str()));
        fragment(&mut out, engine, "logo", Some(engine.logo()));
        fragment(
            &mut out,
            engine,
            "brand",
            Some(&format!("{:06X}", engine.brand())),
        );
        answer(&mut out, engine, "default_port", engine.default_port());
        answer(&mut out, engine, "fields", engine.fields());
        answer(&mut out, engine, "syntax", engine.syntax());
        answer(&mut out, engine, "takes_options", engine.takes_options());
        answer(&mut out, engine, "resolves_srv", engine.resolves_srv());
        answer(
            &mut out,
            engine,
            "assigns_default",
            engine.assigns_default(),
        );
        answer(
            &mut out,
            engine,
            "holds_read_only",
            engine.holds_read_only(),
        );
        answer(
            &mut out,
            engine,
            "schema_switch_waits_for_transaction",
            engine.schema_switch_waits_for_transaction(),
        );
        answer(
            &mut out,
            engine,
            "skips_duplicates",
            engine.skips_duplicates(),
        );
        answer(
            &mut out,
            engine,
            "takes_new_fields",
            engine.takes_new_fields(),
        );
        answer(&mut out, engine, "opened_column", engine.opened_column());
        answer(
            &mut out,
            engine,
            "sorts_on_server",
            engine.sorts_on_server(),
        );
        answer(&mut out, engine, "pages", engine.pages());
        answer(&mut out, engine, "pages_by_key", engine.pages_by_key());
        answer(
            &mut out,
            engine,
            "exact_row_estimates",
            engine.exact_row_estimates(),
        );
        answer(&mut out, engine, "copies_ddl", engine.copies_ddl());
        answer(
            &mut out,
            engine,
            "switches_database",
            engine.switches_database(),
        );
        answer(
            &mut out,
            engine,
            "switches_database_in_session",
            engine.switches_database_in_session(),
        );
        answer(&mut out, engine, "picks_schema", engine.picks_schema());
        answer(
            &mut out,
            engine,
            "edits_query_rows",
            engine.edits_query_rows(),
        );
        fragment(&mut out, engine, "sort_clause", Some(engine.sort_clause()));
        fragment(
            &mut out,
            engine,
            "raw_filter_placeholder",
            Some(engine.raw_filter_placeholder()),
        );
        fragment(
            &mut out,
            engine,
            "raw_filter_label",
            Some(engine.raw_filter_label()),
        );
        fragment(
            &mut out,
            engine,
            "review_label",
            Some(engine.review_label()),
        );
        answer(
            &mut out,
            engine,
            "reads_literal_offsets",
            engine.reads_literal_offsets(),
        );
        answer(
            &mut out,
            engine,
            "backslash_escapes",
            engine.backslash_escapes(),
        );
        for kind in KINDS {
            answer(
                &mut out,
                engine,
                &format!("takes_inserts {kind:?}"),
                engine.takes_inserts(kind),
            );
            answer(
                &mut out,
                engine,
                &format!("takes_imports {kind:?}"),
                engine.takes_imports(kind),
            );
        }
        for name in [
            "postgres",
            "information_schema",
            "mysql",
            "performance_schema",
            "sys",
            "master",
            "tempdb",
            "model",
            "msdb",
            "admin",
            "local",
            "config",
            "SNOWFLAKE",
            "SNOWFLAKE_SAMPLE_DATA",
            "main",
            "snowflake",
            "MYSQL",
        ] {
            answer(
                &mut out,
                engine,
                &format!("is_system_database {name}"),
                engine.is_system_database(name),
            );
        }
        for data_type in [
            "bytea",
            "BLOB",
            "tinyblob",
            "varbinary(16)",
            "binary(4)",
            "image",
            "rowversion",
            "timestamp",
            "bindata",
            "text",
            "int",
            "string",
            "hash",
        ] {
            answer(
                &mut out,
                engine,
                &format!("is_binary_type {data_type}"),
                engine.is_binary_type(data_type),
            );
            answer(
                &mut out,
                engine,
                &format!("writes_back {data_type}"),
                engine.writes_back([data_type]),
            );
        }
        for name in ["type", "key", "id"] {
            answer(
                &mut out,
                engine,
                &format!("new_row_omits {name}"),
                engine.new_row_omits(name),
            );
        }
        for name in ["key", "member", "score", "value", "field", "id"] {
            answer(
                &mut out,
                engine,
                &format!("edits_key_column {name}"),
                engine.edits_key_column(name),
            );
        }
        for default in [
            None,
            Some("GENERATED ALWAYS AS (a + 1) STORED"),
            Some("GENERATED ALWAYS AS ((a))"),
            Some("AUTO_INCREMENT"),
            Some("IDENTITY(1,1)"),
            Some("AS ([a]+(1))"),
            Some("nextval('s'::regclass)"),
            Some("((getdate()))"),
            Some("GENERATED ALWAYS AS IDENTITY"),
            Some("AUTO_INCREMENTS"),
            Some("auto_increment"),
            Some("generated always as (a)"),
        ] {
            answer(
                &mut out,
                engine,
                &format!("generates {}", default.unwrap_or("(no default)")),
                engine.generates(&column(default)),
            );
        }
        for (case, structure) in structures() {
            answer(
                &mut out,
                engine,
                &format!("skips_duplicates_into {case}"),
                engine.skips_duplicates_into(&structure),
            );
        }
        for expression in [
            r#""a""b""#,
            "`a``b`",
            "a",
            r#""x""#,
            "'q'",
            "lower(name)",
            "3",
        ] {
            fragment(
                &mut out,
                engine,
                &format!("unquote_identifier {expression}"),
                Some(&engine.unquote_identifier(expression)),
            );
        }
        for operator in Operator::ALL {
            answer(
                &mut out,
                engine,
                &format!("Operator::on {operator:?}"),
                operator.on(engine),
            );
            answer(
                &mut out,
                engine,
                &format!("Operator::offered {operator:?}"),
                operator.offered(engine, None),
            );
        }
        for name in ["key", "type", "Name"] {
            answer(
                &mut out,
                engine,
                &format!("filters_on {name}"),
                filter::filters_on(engine, name),
            );
        }
        fragment(
            &mut out,
            engine,
            "value_placeholder raw Equals",
            Some(filter::value_placeholder(engine, true, Operator::Equals)),
        );
    }
    let spellings = Engine::schemes().flat_map(|(spelling, _)| {
        [
            spelling.to_string(),
            spelling.to_ascii_uppercase(),
            format!("\t{spelling} "),
        ]
    });
    for spelling in spellings.chain(["nope".to_owned()]) {
        let parsed = match Engine::parse(&spelling) {
            Ok(engine) => engine.as_str().to_owned(),
            Err(message) => format!("error: {message}"),
        };
        out.push_str(&format!("parse |{}| => {parsed}\n", visible(&spelling)));
    }
    check("engines_answers", &out);
}

#[test]
fn form_placeholders_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let placeholders = connection_form::placeholders(engine);
        fragment(&mut out, engine, "name", Some(placeholders.name));
        fragment(&mut out, engine, "host", Some(placeholders.host));
        fragment(&mut out, engine, "port", Some(&placeholders.port));
        fragment(&mut out, engine, "database", Some(placeholders.database));
        fragment(&mut out, engine, "user", Some(placeholders.user));
        fragment(&mut out, engine, "path", Some(placeholders.path));
    }
    check("engines_placeholders", &out);
}
