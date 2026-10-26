use super::{check, fragment, home, refusal, sample_filter, statement_with};
use crate::{
    db::{Engine, ExplainMode, Syntax},
    explorer::{PREVIEW_ROW_LIMIT, preview_sql},
    sql,
};

#[test]
fn explains_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        let (schema, relation) = home(engine);
        let mut bases = vec![
            (
                "plain",
                preview_sql(engine, schema, relation, "", Some(PREVIEW_ROW_LIMIT), 0),
            ),
            (
                "filtered",
                preview_sql(
                    engine,
                    schema,
                    relation,
                    &sample_filter(engine),
                    Some(PREVIEW_ROW_LIMIT),
                    0,
                ),
            ),
        ];
        match engine.syntax() {
            Syntax::Sql => bases.push((
                "delete",
                sql::delete_row(engine, schema, relation, &[("id", "7")], &[]).unwrap(),
            )),
            Syntax::Mongo => {
                let c = r#"db.getCollection("accounts")"#;
                bases.extend([
                    (
                        "out stage",
                        format!(r#"{c}.aggregate([{{"$out": "copy"}}])"#),
                    ),
                    ("delete one", format!("{c}.deleteOne({{_id: 1}})")),
                    ("insert one", format!("{c}.insertOne({{a: 1}})")),
                ]);
            }
            Syntax::Redis => {}
        }
        for mode in ExplainMode::ALL {
            for (case, base) in &bases {
                let label = format!("{case} · {mode:?}");
                match sql::explainable(engine, base) {
                    Ok(()) => fragment(
                        &mut out,
                        engine,
                        &format!("{label} · explainable"),
                        Some("ok"),
                    ),
                    Err(message) => refusal(
                        &mut out,
                        engine,
                        &format!("{label} · explainable"),
                        &message,
                    ),
                }
                let composed = engine.explain(mode, base);
                let typed = composed
                    .as_deref()
                    .map(|text| format!(" typed={:?}", sql::typed_explain(engine, text)))
                    .unwrap_or_default();
                statement_with(&mut out, engine, &label, composed.as_deref(), &typed);
            }
        }
    }
    check("explain", &out);
}

#[test]
fn formats_per_engine() {
    let long = format!("select {}b from t", "a, ".repeat(60));
    let sql_texts: Vec<(&str, String)> = vec![
        (
            "plain",
            "select a,b from t where x=1 -- keep\norder by a".into(),
        ),
        ("two statements", "select 1;select 2;".into()),
        ("two statements spaced", "select 1;\n\n  select 2".into()),
        ("comment only", "-- only a comment".into()),
        ("empty", String::new()),
        ("block comment", "select /* a ; b */ 1".into()),
        ("backslash literal", r"select 'a\b', 'it\'s' from t".into()),
        ("backslash newline", r"select 'a\nb'".into()),
        ("backslash newline escape string", r"select E'a\nb'".into()),
        ("quoted names", r#"select `a`, "b", [c] from t"#.into()),
        (
            "placeholders",
            "select * from t where a = $1 and b = ? and c = :name and d = @p1 and e = %s".into(),
        ),
        ("dollar quote", "select $$a;b$$".into()),
        ("unended string", "select 'abc".into()),
        ("unended comment", "select /* x".into()),
        ("unended identifier", "select \"abc".into()),
        ("very long line", long),
    ];
    let mssql_texts = [
        ("go separator", "select 1\nGO\nselect [x] from [t]"),
        ("go with count", "select 1\ngo 5\nselect 2"),
        ("go inline", "select 1 GO"),
    ];
    let mongo_texts = [
        ("find", r#"db.getCollection("a").find({a:1,b:{$gt:2}})"#),
        (
            "multi-line find",
            "db.getCollection(\"a\")\n  .find({a:1,\n b:{$gt:2}})\n  .limit(5)",
        ),
        ("unterminated", "db.getCollection(\"a\").find({"),
        ("empty", ""),
    ];
    let redis_texts = [("get", "GET k"), ("empty", "")];
    let mut out = String::new();
    for engine in Engine::ALL {
        let texts: Vec<(&str, &str)> = match engine.syntax() {
            Syntax::Sql => {
                let mut texts: Vec<_> = sql_texts.iter().map(|(c, t)| (*c, t.as_str())).collect();
                if engine == Engine::SqlServer {
                    texts.extend(mssql_texts);
                }
                texts
            }
            Syntax::Mongo => mongo_texts.to_vec(),
            Syntax::Redis => redis_texts.to_vec(),
        };
        for (case, text) in texts {
            match sql::format(engine, text) {
                Ok(formatted) => fragment(&mut out, engine, case, Some(&formatted)),
                Err(message) => refusal(&mut out, engine, case, message),
            }
        }
    }
    check("format", &out);
}
