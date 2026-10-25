use super::{PAGE_KEYS, RELATIONS, check, home, sample_filter, statement};
use crate::{
    db::{Engine, RelationKind},
    explorer::{
        PREVIEW_ROW_LIMIT, count_sql, drop_sql, open_row_sql, preview_sql, probe_sql,
        select_top_sql, truncate_sql,
    },
    sql,
};

const KEY_SET_LABELS: [&str; 5] = [
    "no key",
    "one key",
    "composite key",
    "awkward keys",
    "empty name",
];

fn keys(set: &[&str]) -> Vec<String> {
    set.iter().map(|key| (*key).to_owned()).collect()
}

fn record_paged(out: &mut String, engine: Engine, case: &str, preview: &str, set: &[&str]) {
    let paged = sql::paged(engine, preview, &keys(set));
    statement(out, engine, &format!("{case} · paged"), paged.as_deref());
    let unpaged = paged.as_deref().map(sql::unpaged);
    statement(
        out,
        engine,
        &format!("{case} · unpaged"),
        unpaged.as_deref(),
    );
}

#[test]
fn previews_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, schema, relation) in RELATIONS {
            let filtered = sample_filter(engine);
            let variants = [
                ("all", "", None, 0),
                ("page", "", Some(PREVIEW_ROW_LIMIT), 0),
                ("page 3", "", Some(100), 200),
                ("filtered", filtered.as_str(), Some(PREVIEW_ROW_LIMIT), 0),
            ];
            for (variant, filter, limit, offset) in variants {
                let preview = preview_sql(engine, schema, relation, filter, limit, offset);
                let label = format!("{case} · {variant}");
                statement(&mut out, engine, &label, Some(&preview));
                for (set, set_label) in PAGE_KEYS.iter().zip(KEY_SET_LABELS) {
                    record_paged(
                        &mut out,
                        engine,
                        &format!("{label} · {set_label}"),
                        &preview,
                        set,
                    );
                }
            }
        }
        let (schema, relation) = home(engine);
        let q = engine.qualified(schema, relation);
        let others = [
            ("no limit", format!("SELECT * FROM {q}")),
            (
                "limit with order by",
                format!("SELECT * FROM {q} ORDER BY \"id\" LIMIT 5"),
            ),
            (
                "limit and offset, no table",
                "SELECT 1 LIMIT 5 OFFSET 2".to_owned(),
            ),
            ("non-numeric limit", format!("SELECT * FROM {q} LIMIT x")),
        ];
        for (case, text) in others {
            record_paged(&mut out, engine, case, &text, PAGE_KEYS[1]);
        }
    }
    check("previews", &out);
}

#[test]
fn counts_and_probes_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, schema, relation) in RELATIONS {
            let filtered = sample_filter(engine);
            for (variant, filter) in [("all", ""), ("filtered", filtered.as_str())] {
                let count = count_sql(engine, schema, relation, filter);
                statement(
                    &mut out,
                    engine,
                    &format!("{case} · count {variant}"),
                    Some(&count),
                );
                let probe = probe_sql(engine, schema, relation, filter);
                statement(
                    &mut out,
                    engine,
                    &format!("{case} · probe {variant}"),
                    Some(&probe),
                );
            }
        }
    }
    check("previews_counts", &out);
}

#[test]
fn copy_menu_text_per_engine() {
    let kinds = [
        RelationKind::Table,
        RelationKind::PartitionedTable,
        RelationKind::View,
        RelationKind::MaterializedView,
        RelationKind::ForeignTable,
    ];
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, schema, relation) in RELATIONS {
            let top = select_top_sql(engine, schema, relation);
            statement(
                &mut out,
                engine,
                &format!("{case} · select top"),
                top.as_deref(),
            );
            for kind in kinds {
                let drop = drop_sql(engine, schema, relation, kind);
                statement(
                    &mut out,
                    engine,
                    &format!("{case} · drop {kind:?}"),
                    drop.as_deref(),
                );
            }
            let truncate = truncate_sql(engine, schema, relation);
            statement(
                &mut out,
                engine,
                &format!("{case} · truncate"),
                Some(&truncate),
            );
        }
    }
    check("previews_copy", &out);
}

#[test]
fn opened_keys_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for rows in [100, 0] {
            for key in ["k", "a key \"q\""] {
                for ty in [
                    "string",
                    "hash",
                    "list",
                    "set",
                    "zset",
                    "stream",
                    "ReJSON-RL",
                    "TSDB-TYPE",
                    "unknown",
                ] {
                    let cell = |column: &str| match column {
                        "key" => Some(key),
                        "type" => Some(ty),
                        _ => None,
                    };
                    let opened = open_row_sql(engine, rows, cell);
                    statement(
                        &mut out,
                        engine,
                        &format!("rows={rows} key={key} type={ty}"),
                        opened.as_deref(),
                    );
                }
            }
        }
    }
    check("previews_open_row", &out);
}
