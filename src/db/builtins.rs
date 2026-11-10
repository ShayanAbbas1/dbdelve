//! The functions and value expressions each SQL dialect has built in, as data:
//! one table per dialect, read through [`Engine::builtins`](super::Engine::builtins)
//! and offered by completion beside the catalog's own objects.
//!
//! Each name is spelled as the dialect's documentation spells it, so what
//! completion writes reads the way the server's manual does. ClickHouse's
//! names are case-sensitive, so its camelCase is the name, not a style.

pub(super) mod ansi;
pub(super) mod clickhouse;
pub(super) mod duckdb;
pub(super) mod mysql;
pub(super) mod postgres;
pub(super) mod snowflake;
pub(super) mod sqlite;
pub(super) mod sqlserver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinKind {
    Function,
    Aggregate,
    Window,
    /// Written without parentheses: `NULL`, `TRUE`, `CURRENT_DATE`.
    Value,
}

#[derive(Clone, Copy, Debug)]
pub struct Builtin {
    pub name: &'static str,
    pub kind: BuiltinKind,
    /// The parameters without their parentheses, optional ones in brackets:
    /// `"x numeric [, n int]"`. Empty for a function that takes none, and for
    /// a value.
    pub args: &'static str,
    pub returns: &'static str,
    /// One short sentence.
    pub doc: &'static str,
}

const fn func(
    name: &'static str,
    args: &'static str,
    returns: &'static str,
    doc: &'static str,
) -> Builtin {
    Builtin {
        name,
        kind: BuiltinKind::Function,
        args,
        returns,
        doc,
    }
}

const fn agg(
    name: &'static str,
    args: &'static str,
    returns: &'static str,
    doc: &'static str,
) -> Builtin {
    Builtin {
        name,
        kind: BuiltinKind::Aggregate,
        args,
        returns,
        doc,
    }
}

const fn win(
    name: &'static str,
    args: &'static str,
    returns: &'static str,
    doc: &'static str,
) -> Builtin {
    Builtin {
        name,
        kind: BuiltinKind::Window,
        args,
        returns,
        doc,
    }
}

const fn val(name: &'static str, returns: &'static str, doc: &'static str) -> Builtin {
    Builtin {
        name,
        kind: BuiltinKind::Value,
        args: "",
        returns,
        doc,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Engine;

    #[test]
    fn no_dialect_offers_a_name_twice() {
        for engine in Engine::ALL {
            let mut names: Vec<String> = engine
                .builtins()
                .iter()
                .map(|builtin| builtin.name.to_ascii_lowercase())
                .collect();
            names.sort();
            let duplicates: Vec<_> = names.windows(2).filter(|pair| pair[0] == pair[1]).collect();
            assert!(duplicates.is_empty(), "{engine:?} repeats {duplicates:?}");
        }
    }
}
