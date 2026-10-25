use super::{PAGE_KEYS, RELATIONS, check, fragment, home, sample_filter, statement};
use crate::{
    db::{Column, Engine},
    explorer::{PREVIEW_ROW_LIMIT, preview_sql},
    filter::{relation_sql, sort_expression},
    sql::{self, SortKey},
};

fn columns(names: &[&str]) -> Vec<Column> {
    names
        .iter()
        .map(|name| Column {
            name: (*name).to_owned(),
            data_type: None,
        })
        .collect()
}

/// The four key sets, each `None` where the engine writes no expression for
/// one of its columns (Redis sorts in memory only).
fn key_sets(engine: Engine) -> Vec<(&'static str, Option<Vec<SortKey>>)> {
    let columns = columns(&["id", "Name"]);
    let key = |index, ascending| {
        sort_expression(engine, &columns, index)
            .map(|expression| SortKey::new(expression, ascending))
    };
    let both = key(0, true).zip(key(1, false)).map(|(a, b)| vec![a, b]);
    vec![
        ("[]", Some(vec![])),
        ("[id asc]", key(0, true).map(|key| vec![key])),
        ("[Name desc]", key(1, false).map(|key| vec![key])),
        ("[id asc, Name desc]", both),
    ]
}

#[test]
fn sort_expressions_per_engine() {
    let mut out = String::new();
    let lists: [(&str, Vec<Column>, usize); 3] = [
        (
            "columns",
            columns(&["id", "Name", "we\"ird", "back`tick", "ümlaut", "select"]),
            7,
        ),
        ("duplicates", columns(&["id", "id"]), 2),
        ("empty name", columns(&[""]), 1),
    ];
    for engine in Engine::ALL {
        for (label, list, indexes) in &lists {
            for index in 0..*indexes {
                let expression = sort_expression(engine, list, index);
                let name = list.get(index).map_or("out of range", |c| c.name.as_str());
                fragment(
                    &mut out,
                    engine,
                    &format!("{label} #{index} {name}"),
                    expression.as_deref(),
                );
            }
        }
    }
    check("sorts_expressions", &out);
}

fn sql_statements(engine: Engine) -> Vec<(&'static str, String)> {
    let (schema, relation) = home(engine);
    let q = engine.qualified(schema, relation);
    let filtered = sample_filter(engine);
    vec![
        (
            "preview",
            preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 0),
        ),
        (
            "preview page 3",
            preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 200),
        ),
        (
            "filtered",
            preview_sql(
                engine,
                schema,
                relation,
                &filtered,
                Some(PREVIEW_ROW_LIMIT),
                0,
            ),
        ),
        ("existing order", format!("SELECT * FROM {q} ORDER BY 1")),
        (
            "cte",
            "WITH x AS (SELECT 1 AS a) SELECT * FROM x".to_owned(),
        ),
        ("trailing comment", format!("SELECT * FROM {q} -- note")),
        ("union", "SELECT 1 AS a UNION SELECT 2".to_owned()),
        ("unparseable", "SELECT * FROM".to_owned()),
        ("update", format!("UPDATE {q} SET a = 1")),
    ]
}

fn mongo_statements() -> Vec<(&'static str, String)> {
    let engine = Engine::MongoDb;
    let (schema, relation) = home(engine);
    let c = r#"db.getCollection("accounts")"#;
    let filtered = sample_filter(engine);
    vec![
        (
            "find",
            preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 0),
        ),
        (
            "find skip",
            preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 200),
        ),
        (
            "find filtered",
            preview_sql(
                engine,
                schema,
                relation,
                &filtered,
                Some(PREVIEW_ROW_LIMIT),
                0,
            ),
        ),
        (
            "find sorted",
            format!(r#"{c}.find({{}}).sort({{"id": -1}})"#),
        ),
        (
            "find sorted, paged",
            format!(r#"{c}.find({{}}).sort({{"id": 1}}).skip(200).limit(1000)"#),
        ),
        (
            "find sorted on own line",
            format!("{c}.find({{}})\n  .sort({{\"id\": -1}})\n  .limit(1000)"),
        ),
        (
            "find sorted, two keys",
            format!(r#"{c}.find({{}}).sort({{"id": 1, "Name": -1}})"#),
        ),
        ("aggregate empty", format!("{c}.aggregate([])")),
        (
            "aggregate no sort",
            format!(r#"{c}.aggregate([{{"$match": {{}}}}])"#),
        ),
        (
            "aggregate paging only",
            format!(r#"{c}.aggregate([{{"$match": {{}}}}, {{"$skip": 5}}, {{"$limit": 10}}])"#),
        ),
        (
            "aggregate sort only",
            format!(r#"{c}.aggregate([{{"$sort": {{"id": 1}}}}])"#),
        ),
        (
            "aggregate sort last",
            format!(r#"{c}.aggregate([{{"$match": {{}}}}, {{"$sort": {{"id": 1}}}}])"#),
        ),
        (
            "aggregate sort then paging",
            format!(
                r#"{c}.aggregate([{{"$sort": {{"id": 1}}}}, {{"$skip": 5}}, {{"$limit": 10}}])"#
            ),
        ),
        (
            "aggregate sort mid-pipeline",
            format!(r#"{c}.aggregate([{{"$sort": {{"id": 1}}}}, {{"$match": {{}}}}])"#),
        ),
        (
            "aggregate other stages",
            format!(r#"{c}.aggregate([{{"$group": {{"_id": "$a"}}}}])"#),
        ),
        ("not a query", format!("{c}.insertOne({{}})")),
    ]
}

#[test]
fn order_by_splices_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let statements = match engine {
            Engine::MongoDb => mongo_statements(),
            Engine::Redis => {
                let (schema, relation) = home(engine);
                vec![(
                    "preview",
                    preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 0),
                )]
            }
            _ => sql_statements(engine),
        };
        for (set, keys) in key_sets(engine) {
            let Some(keys) = keys else {
                fragment(&mut out, engine, &format!("key set {set}"), None);
                continue;
            };
            for (case, text) in &statements {
                let spliced = sql::with_order_by(engine, text, &keys);
                statement(
                    &mut out,
                    engine,
                    &format!("{case} · {set}"),
                    spliced.as_deref(),
                );
            }
        }
    }
    check("sorts_order_by", &out);
}

#[test]
fn sorted_pages_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, schema, relation) in RELATIONS {
            let filtered = sample_filter(engine);
            for (set, keys) in key_sets(engine) {
                let Some(keys) = keys else { continue };
                for (filter_label, filter) in [("none", ""), ("filtered", filtered.as_str())] {
                    for offset in [0, 200] {
                        let page = relation_sql(
                            engine,
                            schema,
                            relation,
                            filter,
                            &keys,
                            Some(PREVIEW_ROW_LIMIT),
                            offset,
                        );
                        let label = format!("{case} · {filter_label} · {set} · offset {offset}");
                        statement(&mut out, engine, &label, Some(&page));
                        for key in [PAGE_KEYS[1], PAGE_KEYS[2]] {
                            let key_label = key.join("+");
                            let key: Vec<String> = key.iter().map(|k| (*k).to_owned()).collect();
                            let paged = sql::paged(engine, &page, &key);
                            statement(
                                &mut out,
                                engine,
                                &format!("{label} · paged {key_label}"),
                                paged.as_deref(),
                            );
                            let unpaged = paged.as_deref().map(sql::unpaged);
                            statement(
                                &mut out,
                                engine,
                                &format!("{label} · unpaged {key_label}"),
                                unpaged.as_deref(),
                            );
                        }
                    }
                }
            }
        }
    }
    check("sorts_pages", &out);
}
