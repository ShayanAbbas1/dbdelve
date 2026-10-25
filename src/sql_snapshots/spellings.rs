use super::{NAMES, RELATIONS, TYPES, VALUES, check, fragment, long_text, refusal};
use crate::db::{Engine, ExplainMode};

fn literals() -> Vec<(&'static str, String)> {
    let mut all: Vec<_> = VALUES
        .iter()
        .map(|(case, value)| (*case, (*value).to_owned()))
        .collect();
    all.push(("long", long_text()));
    all
}

#[test]
fn identifiers_are_quoted_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for name in NAMES {
            let quoted = engine.quote_identifier(name);
            fragment(&mut out, engine, &format!("ident {name}"), Some(&quoted));
        }
        for (case, schema, relation) in RELATIONS {
            let qualified = engine.qualified(schema, relation);
            fragment(
                &mut out,
                engine,
                &format!("qualified {case}"),
                Some(&qualified),
            );
        }
    }
    check("spellings_identifiers", &out);
}

#[test]
fn literals_are_quoted_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, value) in literals() {
            fragment(&mut out, engine, case, Some(&engine.quote_literal(&value)));
        }
    }
    check("spellings_literals", &out);
}

#[test]
fn values_are_spelled_per_column_type() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (case, value) in VALUES {
            for ty in TYPES {
                let quoted = engine.quote_value(value, ty);
                fragment(
                    &mut out,
                    engine,
                    &format!("{case} :: {}", ty.unwrap_or("none")),
                    Some(&quoted),
                );
            }
        }
    }
    check("spellings_values_by_type", &out);
}

#[test]
fn engine_spellings_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for value in [true, false] {
            fragment(
                &mut out,
                engine,
                &format!("boolean_literal {value}"),
                Some(engine.boolean_literal(value)),
            );
        }
        fragment(&mut out, engine, "count_all", Some(engine.count_all()));
        fragment(
            &mut out,
            engine,
            "transaction_start",
            engine.transaction_start(),
        );
        match engine.all_or_nothing() {
            Ok(text) => fragment(&mut out, engine, "all_or_nothing", Some(text)),
            Err(message) => refusal(&mut out, engine, "all_or_nothing", message),
        }
        for mode in ExplainMode::ALL {
            fragment(
                &mut out,
                engine,
                &format!("explain_prefix {mode:?}"),
                engine.explain_prefix(mode),
            );
            fragment(
                &mut out,
                engine,
                &format!("explain_suffix {mode:?}"),
                Some(engine.explain_suffix(mode)),
            );
        }
    }
    check("spellings_engine", &out);
}
