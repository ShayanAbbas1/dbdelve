//! Writing a whole schema to one file of SQL: each table's `CREATE`, then each
//! table's rows, in an order the file runs back in.

use crate::db::ForeignKey;

/// The `tables` of `schema`, each after the ones its keys point at, or the ones
/// caught in a cycle, which no order runs back. A key to anything this export
/// does not write holds nothing back: the file cannot create it either way.
/// Where nothing forces an order, the tables keep the one they came in, so the
/// file reads as the catalog does.
///
/// ponytail: a cycle, a table pointing at itself among them, is refused until
/// #188 settles how a file replays one (its keys added after the rows, or the
/// checks turned off inside the file).
pub(super) fn dependency_order(
    schema: &str,
    tables: Vec<(String, Vec<ForeignKey>)>,
) -> Result<Vec<String>, Vec<String>> {
    let names: Vec<String> = tables.iter().map(|(name, _)| name.clone()).collect();
    let mut placed: Vec<String> = Vec::with_capacity(tables.len());
    let mut waiting = tables;
    while !waiting.is_empty() {
        let before = waiting.len();
        waiting.retain(|(name, keys)| {
            let ready = keys.iter().all(|key| {
                key.referenced_schema != schema
                    || !names.contains(&key.referenced_table)
                    || (key.referenced_table != *name && placed.contains(&key.referenced_table))
            });
            if ready {
                placed.push(name.clone());
            }
            !ready
        });
        if waiting.len() == before {
            return Err(waiting.into_iter().map(|(name, _)| name).collect());
        }
    }
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(table: &str) -> ForeignKey {
        ForeignKey {
            column: "ref_id".into(),
            referenced_schema: "s".into(),
            referenced_table: table.into(),
            referenced_column: "id".into(),
        }
    }

    #[test]
    fn a_table_comes_after_the_tables_it_references_and_ties_keep_their_order() {
        let tables = vec![
            ("orders".into(), vec![key("customers"), key("products")]),
            ("products".into(), vec![]),
            ("customers".into(), vec![]),
            ("lines".into(), vec![key("orders"), key("products")]),
        ];
        assert_eq!(
            dependency_order("s", tables),
            Ok(vec![
                "products".into(),
                "customers".into(),
                "orders".into(),
                "lines".into()
            ])
        );
    }

    #[test]
    fn tables_that_reference_each_other_or_themselves_are_named_and_refused() {
        let pair = vec![
            ("plain".into(), vec![]),
            ("employees".into(), vec![key("departments")]),
            ("departments".into(), vec![key("employees")]),
        ];
        assert_eq!(
            dependency_order("s", pair),
            Err(vec!["employees".into(), "departments".into()])
        );
        let own = vec![("employees".into(), vec![key("employees")])];
        assert_eq!(dependency_order("s", own), Err(vec!["employees".into()]));
    }

    #[test]
    fn a_key_to_anything_outside_the_export_does_not_hold_a_table_back() {
        let mut elsewhere = key("customers");
        elsewhere.referenced_schema = "other".into();
        let tables = vec![("orders".into(), vec![elsewhere, key("a_view")])];
        assert_eq!(dependency_order("s", tables), Ok(vec!["orders".into()]));
    }
}
