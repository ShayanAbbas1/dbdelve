//! What the editor offers while you type.
//!
//! gpui-component's input already owns the whole completion surface — the
//! popup under the caret, its scroll, the arrow keys, `enter` to accept and
//! `escape` to dismiss — behind one trait with two required methods. None of
//! it needs a language server; `lsp_types` is only the vocabulary. So this
//! module is the part that is dbdelve's: which identifiers are worth offering
//! for the text in front of the cursor.
//!
//! Every object offered comes from the catalog the explorer already loaded, so
//! the popup can never name a relation the tree does not show; the rest is the
//! dialect's keywords and built-in functions and values. Nothing here
//! runs SQL and nothing here edits the buffer: an accepted row is a
//! [`TextEdit`] over the word being typed, and the input applies it.
//!
//! **This is a lexer, not a parser.** Which identifiers make sense at a
//! caret is decided by the last keyword before it and by the relations named
//! in the statement, both found by scanning tokens. A parse tree would be the
//! principled answer and is the wrong tool here: half-typed SQL is a parse
//! error by definition — `SELECT * FROM ` is exactly the text a user most
//! wants completed and exactly the text `tree_sitter_sequel` returns an
//! `ERROR` node for. The one job a scan genuinely cannot do is know it is
//! inside a string literal, and [`suppressed`] does that job on its own.

use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, sync::Arc};

use gpui::{App, Result, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Documentation, MarkupContent, MarkupKind, TextEdit,
};

use crate::{
    Workspace,
    db::{
        Builtin, BuiltinKind, Catalog, Engine, ForeignKey, Reference, RelationKind, Structure,
        Syntax,
    },
    fuzzy::{Fuzzy, Query},
    mql::{self, Completing},
    redis_cmd, sql,
};

/// What is known about one relation's columns.
///
/// The catalog does not carry them. Fetching every column of every relation at
/// connect was the first implementation and it does not scale: on a large
/// schema that is a multi-million-row result, buffered whole by the driver
/// before dbdelve sees a row, then held for the life of the connection and
/// duplicated into this provider's snapshot -- all of it paid before the user
/// has typed anything, and most of it for relations they will never mention.
///
/// So a relation's columns are fetched when a statement first names it, and
/// kept. What a session holds is bounded by what it actually wrote about.
#[derive(Clone, Debug)]
pub enum ColumnState {
    /// Asked for, not back yet, and how many attempts have already failed.
    /// Held so a relation is asked about once rather than on every keystroke
    /// until it lands.
    Loading(u8),
    Loaded(Described),
    /// That many attempts have failed. Retried while it is under
    /// [`FETCH_ATTEMPTS`], and then not again for this connection.
    ///
    /// Neither extreme is right here. Never retrying means one blip -- or one
    /// statement timeout, which bounds dbdelve's own catalog queries too -- kills
    /// completion for that relation silently, for the rest of the session.
    /// Always retrying means a describe per keystroke, and every one of them
    /// queues on the connection mutex behind the last, so a slow failure would
    /// freeze the profile rather than degrade it.
    Failed(u8),
}

/// What a describe told completion about one relation: its columns, and the
/// foreign keys a `JOIN` onto it or from it would follow.
#[derive(Clone, Debug, Default)]
pub struct Described {
    pub columns: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    /// Empty when the describe did not ask who points back, which it only
    /// does while joins are being suggested.
    pub referenced_by: Vec<Reference>,
}

impl From<&Structure> for Described {
    fn from(structure: &Structure) -> Self {
        Self {
            columns: structure
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            foreign_keys: structure.foreign_keys.clone(),
            referenced_by: structure.referenced_by.clone(),
        }
    }
}

/// How many times a relation's columns are asked for before dbdelve stops.
const FETCH_ATTEMPTS: u8 = 3;

/// How many relations' columns may be asked for at once. Each describe queues
/// on the session's one lock, so more in flight are no sooner back and only
/// hold more of the blocking pool, which a name in a few hundred schemas would
/// otherwise fill. One left over is asked for on a later keystroke.
const FETCHES_IN_FLIGHT: usize = 4;

/// Columns by schema and relation, shared between the provider that reads them
/// and the workspace that fills them. `Rc` rather than `Arc` because both live
/// on the foreground thread; the fetch itself is a background task that hands
/// its result back before it touches this.
pub type ColumnCache = Rc<RefCell<HashMap<(String, String), ColumnState>>>;

/// Schema and relation pairs whose columns a completion wanted and the cache
/// did not hold.
type Wanted = Vec<(String, String)>;

/// How many rows the popup is allowed. The list is already ranked, so the tail
/// of a 4,000-column database is not what anyone is scrolling for.
const MAX_ITEMS: usize = 50;

/// The keywords worth completing: the ones that carry a statement's shape.
///
/// Deliberately not the full reserved-word list of three engines. A completion
/// list is a menu, and a menu of four hundred keywords is a wall — these are
/// the ones long enough to be worth not typing and common enough to be worth
/// ranking above a column that happens to fuzzy-match.
const KEYWORDS: &[&str] = &[
    "AND",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COALESCE",
    "CROSS JOIN",
    "DESC",
    "DISTINCT",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FROM",
    "FULL JOIN",
    "GROUP BY",
    "HAVING",
    "ILIKE",
    "IN",
    "INNER JOIN",
    "INSERT INTO",
    "INTERSECT",
    "IS NOT NULL",
    "IS NULL",
    "JOIN",
    "LATERAL",
    "LEFT JOIN",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "NULLS FIRST",
    "NULLS LAST",
    "OFFSET",
    "ON",
    "OR",
    "ORDER BY",
    "OVER",
    "PARTITION BY",
    "RETURNING",
    "RIGHT JOIN",
    "SELECT",
    "SET",
    "THEN",
    "TRUE",
    "UNION",
    "UNION ALL",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

/// Keywords after which an identifier names a relation.
const SOURCE_KEYWORDS: &[&str] = &["from", "join", "update", "into", "table"];

/// A completion the popup can offer, before it is ranked.
struct Candidate {
    label: String,
    /// The grey text beside the label — the schema a relation is in, the
    /// relation a column belongs to. What tells two `id`s apart.
    detail: Option<String>,
    kind: CompletionItemKind,
    /// Whether accepting it writes the relation under the schema in `detail`.
    /// A flag rather than the name, which is spelled only for the few that
    /// survive ranking; ranking reads the label alone either way.
    qualified: bool,
    /// The built-in it offers, whose call and documentation are spelled, like
    /// a qualified name, only for the few that survive ranking.
    builtin: Option<&'static Builtin>,
    /// The alias a relation is written with, where one belongs.
    alias: Option<String>,
    /// The condition that joins it to a relation the statement already
    /// holds, spelled with that alias.
    on: Option<String>,
}

impl Candidate {
    fn new(label: impl Into<String>, detail: Option<String>, kind: CompletionItemKind) -> Self {
        Self {
            label: label.into(),
            detail,
            kind,
            qualified: false,
            builtin: None,
            alias: None,
            on: None,
        }
    }

    fn qualified(self, qualified: bool) -> Self {
        Self { qualified, ..self }
    }

    fn builtin(builtin: &'static Builtin) -> Self {
        let (detail, kind) = match builtin.kind {
            BuiltinKind::Function => ("function", CompletionItemKind::FUNCTION),
            BuiltinKind::Aggregate => ("aggregate", CompletionItemKind::FUNCTION),
            BuiltinKind::Window => ("window", CompletionItemKind::FUNCTION),
            BuiltinKind::Value => ("value", CompletionItemKind::CONSTANT),
        };
        Self {
            builtin: Some(builtin),
            ..Self::new(builtin.name, Some(detail.to_string()), kind)
        }
    }

    /// What accepting it writes into the buffer.
    ///
    /// A function is written up to its open parenthesis, ready for the first
    /// argument, and closed as well when it takes none.
    fn written(&self, engine: Engine) -> String {
        if let Some(builtin) = self.builtin {
            return match (builtin.kind, builtin.args) {
                (BuiltinKind::Value, _) => builtin.name.to_string(),
                (_, "") => format!("{}()", builtin.name),
                _ => format!("{}(", builtin.name),
            };
        }
        let name = match (self.qualified, &self.detail) {
            (true, Some(schema)) => format!(
                "{}.{}",
                engine.quote_identifier_if_needed(schema),
                engine.quote_identifier_if_needed(&self.label)
            ),
            _ => self.label.clone(),
        };
        format!("{name}{}", self.aliasing())
    }

    /// The alias and join condition that follow the name, in the buffer and
    /// in the popup alike, so the row shows what accepting it writes.
    fn aliasing(&self) -> String {
        match (&self.alias, &self.on) {
            (Some(alias), Some(on)) => format!(" {alias} ON {on}"),
            (Some(alias), None) => format!(" {alias}"),
            _ => String::new(),
        }
    }

    /// A built-in's sentence over its signature.
    ///
    /// The sentence leads because a menu too narrow to sit beside its
    /// documentation shows only the first line of it. The fence names no
    /// language: a language's tag is drawn over the end of a wrapped line, and
    /// its highlighter colours `with` in `timestamp with time zone` as a
    /// keyword.
    fn documentation(&self) -> Option<Documentation> {
        let builtin = self.builtin?;
        let signature = match builtin.kind {
            BuiltinKind::Value => format!("{} → {}", builtin.name, builtin.returns),
            _ => format!("{}({}) → {}", builtin.name, builtin.args, builtin.returns),
        };
        Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("{}\n\n```\n{signature}\n```", builtin.doc),
        }))
    }
}

/// The completion source for a connected profile's editor.
///
/// Holds a snapshot rather than a live handle. The catalog is replaced whole
/// when it reloads, and so is this — see `Workspace::install_completions`.
pub struct SchemaCompletions {
    engine: Engine,
    catalog: Arc<Catalog>,
    columns: ColumnCache,
    qualify: bool,
    /// Whether a relation completed after `FROM` or `JOIN` is given an alias,
    /// and one after `JOIN` the condition its foreign keys imply.
    alias: bool,
    /// Who to ask when a relation's columns are wanted and not yet held.
    ///
    /// `None` only in this module's own tests, which drive [`Self::items`]
    /// against a pre-filled cache and never reach the fetch.
    workspace: Option<WeakEntity<Workspace>>,
    /// The matcher scores through `&mut self` and the trait hands us `&self`.
    /// One matcher reused rather than one per keystroke, as the palette does.
    fuzzy: RefCell<Fuzzy>,
}

impl SchemaCompletions {
    pub fn new(
        engine: Engine,
        catalog: Arc<Catalog>,
        columns: ColumnCache,
        qualify: bool,
        alias: bool,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        Self {
            engine,
            catalog,
            columns,
            qualify,
            alias,
            workspace: Some(workspace),
            fuzzy: RefCell::new(Fuzzy::new()),
        }
    }

    /// The columns of a relation named anywhere in the catalog, as far as they
    /// are known, recording into `wanted` any this cache has never been asked
    /// for.
    ///
    /// Unqualified, so two schemas with a `users` each contribute both sets.
    /// Offering a column that turns out to belong to the other schema's table
    /// is a worse answer than offering nothing only if the user cannot see
    /// which — which is what `detail` is for.
    ///
    /// A name the catalog does not list is never recorded. The fetch would ask
    /// the server to describe something it has already said does not exist,
    /// once per keystroke, for as long as the typo is on screen.
    fn columns_of(&self, relation: &str, wanted: &mut Vec<(String, String)>) -> Vec<Candidate> {
        self.columns_of_except(relation, None, wanted)
    }

    /// [`Self::columns_of`], less the one relation `except` names, whose
    /// columns the caller has already offered.
    fn columns_of_except(
        &self,
        relation: &str,
        except: Option<&(String, String)>,
        wanted: &mut Wanted,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for schema in &self.catalog.schemas {
            for named in schema
                .relations
                .iter()
                .filter(|named| named.name.eq_ignore_ascii_case(relation))
            {
                let key = (schema.name.clone(), named.name.clone());
                if except != Some(&key) {
                    candidates.extend(self.columns_at(key, wanted));
                }
            }
        }
        candidates
    }

    /// The columns of exactly this schema's relation, for a caller that knows
    /// which one it means. Nothing for one the catalog does not list, for the
    /// reason [`Self::columns_of`] gives.
    fn columns_in(
        &self,
        (schema, relation): &(String, String),
        wanted: &mut Wanted,
    ) -> Vec<Candidate> {
        let listed = self.catalog.schemas.iter().any(|listed| {
            &listed.name == schema && listed.relations.iter().any(|named| &named.name == relation)
        });
        match listed {
            true => self.columns_at((schema.clone(), relation.clone()), wanted),
            false => Vec::new(),
        }
    }

    /// The columns held for one relation, recording it into `wanted` when
    /// they are not.
    ///
    /// Recorded once however often a statement names it: each entry is a
    /// describe on the wire, and a second would reset the first's count of
    /// failed attempts.
    fn columns_at(&self, key: (String, String), wanted: &mut Wanted) -> Vec<Candidate> {
        let relation = key.1.clone();
        self.described(key, wanted, |described| {
            described
                .columns
                .iter()
                .map(|column| {
                    Candidate::new(column, Some(relation.clone()), CompletionItemKind::FIELD)
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// What is held about one relation, read through `read`, recording it
    /// into `wanted` when nothing is.
    fn described<T>(
        &self,
        key: (String, String),
        wanted: &mut Wanted,
        read: impl FnOnce(&Described) -> T,
    ) -> Option<T> {
        let mut want = |key| {
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        };
        match self.columns.borrow().get(&key) {
            Some(ColumnState::Loaded(described)) => return Some(read(described)),
            // Failed, but not yet often enough to give up on.
            Some(ColumnState::Failed(attempts)) if *attempts < FETCH_ATTEMPTS => want(key),
            // In flight, or asked for as often as it is going to be.
            Some(ColumnState::Loading(_) | ColumnState::Failed(_)) => {}
            None => want(key),
        }
        None
    }

    /// The catalog's own spelling of a relation a foreign key names. A key
    /// whose schema the catalog spells differently still finds its relation
    /// when only one has that name.
    fn listed(&self, schema: &str, relation: &str) -> Option<(String, String)> {
        let mut named = self.catalog.schemas.iter().flat_map(|listed| {
            listed
                .relations
                .iter()
                .filter(|named| named.name == relation)
                .map(|named| (listed.name.clone(), named.name.clone()))
        });
        let first = named.next()?;
        let rest = named.collect::<Vec<_>>();
        std::iter::once(&first)
            .chain(&rest)
            .find(|(listed, _)| listed == schema)
            .cloned()
            .or(rest.is_empty().then_some(first))
    }

    /// The relations a foreign key joins to the ones `head`, the statement up
    /// to the caret, already holds, in either direction.
    ///
    /// Not one the statement holds already: after `orders o JOIN order_items
    /// oi` every key between the two would offer the other one again. A key
    /// from a relation to itself is the exception, being the only way to ask
    /// for a self-join.
    fn joins(&self, head: &str, wanted: &mut Wanted) -> Vec<Join> {
        let sources = sources(&reach(head));
        let held = |(schema, relation): &(String, String)| {
            sources.iter().any(|source| {
                source.relation.eq_ignore_ascii_case(relation)
                    && source
                        .schema
                        .as_ref()
                        .is_none_or(|named| named.eq_ignore_ascii_case(schema))
            })
        };
        let mut joins: Vec<Join> = Vec::new();
        for source in &sources {
            let keys = self
                .catalog
                .schemas
                .iter()
                .filter(|schema| {
                    source
                        .schema
                        .as_ref()
                        .is_none_or(|named| named.eq_ignore_ascii_case(&schema.name))
                })
                .flat_map(|schema| {
                    schema
                        .relations
                        .iter()
                        .filter(|named| named.name.eq_ignore_ascii_case(&source.relation))
                        .map(|named| (schema.name.clone(), named.name.clone()))
                })
                .collect::<Vec<_>>();
            for key in keys {
                let found = self.described(key.clone(), wanted, |described| {
                    let mut found = foreign_keys(&described.foreign_keys);
                    found.extend(described.referenced_by.iter().map(|reference| {
                        (
                            (reference.schema.clone(), reference.table.clone()),
                            vec![(
                                reference.column.clone(),
                                reference.referenced_column.clone(),
                            )],
                        )
                    }));
                    found
                });
                for ((schema, relation), columns) in found.unwrap_or_default() {
                    let Some(target) = self.listed(&schema, &relation) else {
                        continue;
                    };
                    if held(&target) && target != key {
                        continue;
                    }
                    let join = Join {
                        target,
                        onto: source.alias.clone().unwrap_or(source.relation.clone()),
                        columns,
                    };
                    if !joins.contains(&join) {
                        joins.push(join);
                    }
                }
            }
        }
        joins
    }

    /// `relations` as the caret wants them written: each with an alias where
    /// one belongs, led after `JOIN` by the ones a foreign key joins to the
    /// relations already held, with the condition.
    fn aliased(
        &self,
        around: &Around,
        relations: Vec<Candidate>,
        scope: Option<&(String, String)>,
        wanted: &mut Wanted,
    ) -> Vec<Candidate> {
        // A filter bar's scope is a relation the text never names, so there
        // is nothing an alias there could be used for.
        if !self.alias || scope.is_some() || !around.takes_alias() {
            return relations;
        }
        // Without the word being typed, which `FROM o` would otherwise bind
        // as a relation called `o` and cost `orders` its alias.
        let taken = bindings(&format!("{} {}", around.head, around.tail))
            .into_iter()
            .map(|(binding, _)| binding)
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        if around.takes_condition() {
            for join in self.joins(around.head, wanted) {
                let Some(relation) = relations.iter().find(|relation| {
                    relation.label == join.target.1
                        && relation.detail.as_ref() == Some(&join.target.0)
                }) else {
                    continue;
                };
                let alias = alias_for(&relation.label, &taken);
                let on = join
                    .columns
                    .iter()
                    .map(|(theirs, ours)| format!("{alias}.{theirs} = {}.{ours}", join.onto))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                candidates.push(Candidate {
                    alias: Some(alias),
                    on: Some(on),
                    ..Candidate::new(&relation.label, relation.detail.clone(), relation.kind)
                        .qualified(relation.qualified)
                });
            }
        }
        candidates.extend(relations.into_iter().map(|relation| Candidate {
            alias: Some(alias_for(&relation.label, &taken)),
            ..relation
        }));
        candidates
    }

    fn relations_in(&self, schema_name: &str) -> Vec<Candidate> {
        self.catalog
            .schemas
            .iter()
            .filter(|schema| schema.name.eq_ignore_ascii_case(schema_name))
            .flat_map(|schema| {
                schema.relations.iter().map(|rel| {
                    Candidate::new(
                        &rel.name,
                        Some(schema.name.clone()),
                        relation_kind(rel.kind),
                    )
                })
            })
            .collect()
    }

    fn all_relations(&self, qualify: bool) -> Vec<Candidate> {
        self.catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema.relations.iter().map(move |rel| Candidate {
                    qualified: qualify,
                    ..Candidate::new(
                        &rel.name,
                        Some(schema.name.clone()),
                        relation_kind(rel.kind),
                    )
                })
            })
            .collect()
    }

    fn all_routines(&self) -> Vec<Candidate> {
        self.catalog
            .schemas
            .iter()
            .flat_map(|schema| {
                schema.routines.iter().map(|routine| {
                    Candidate::new(
                        &routine.name,
                        Some(schema.name.clone()),
                        CompletionItemKind::FUNCTION,
                    )
                })
            })
            .collect()
    }

    fn schema_names(&self) -> Vec<Candidate> {
        self.catalog
            .schemas
            .iter()
            .map(|schema| Candidate::new(&schema.name, None, CompletionItemKind::MODULE))
            .collect()
    }

    /// What to offer, in preference order, before ranking narrows it.
    ///
    /// Order matters twice over: the scorer's ties are broken by it, and an
    /// empty query scores everything equally, so this order *is* the list the
    /// user sees after a `.`.
    fn candidates(
        &self,
        around: &Around,
        qualifier: Option<&str>,
        scope: Option<&(String, String)>,
        wanted: &mut Vec<(String, String)>,
    ) -> Vec<Candidate> {
        let Around {
            statement, before, ..
        } = *around;
        if let Some(qualifier) = qualifier {
            // `x.` — `x` is an alias if the statement bound one, otherwise the
            // relation or schema it looks like. An alias wins: a query that
            // says `FROM orders o` has made `o` mean something here, and a
            // table elsewhere in the database called `o` has not.
            let relation = bindings(statement)
                .into_iter()
                .find(|(binding, _)| binding.eq_ignore_ascii_case(qualifier))
                .map(|(_, relation)| relation)
                .unwrap_or_else(|| qualifier.to_string());

            let columns = self.columns_of(&relation, wanted);
            if !columns.is_empty() {
                return columns;
            }
            // Nothing held for it yet, which may only mean a fetch that has not
            // landed. Offering the schema's relations is the right answer when
            // the qualifier is a schema, and harmless when it is not: an
            // unknown name matches nothing.
            return self.aliased(around, self.relations_in(qualifier), scope, wanted);
        }

        // A qualifier `qualifier_before` cannot read, `"audit".` say, is still
        // one, and qualifying after it would write a second.
        let qualify = self.qualify && !before.ends_with('.');
        if expects_relation(before) {
            let mut candidates = self.aliased(around, self.all_relations(qualify), scope, wanted);
            candidates.extend(self.schema_names());
            return candidates;
        }

        // Columns of the relations this statement actually names come first:
        // in a statement about `accounts`, `email` is a better guess than
        // every other `email` in the database, and than `EXISTS`. The scope's
        // columns lead them, as the relation the text is about without naming
        // it, and are not offered again when the text names it too.
        let mut candidates = scope
            .map(|scope| self.columns_in(scope, wanted))
            .unwrap_or_default();
        candidates.extend(
            bindings(statement)
                .into_iter()
                .map(|(_, relation)| relation)
                .fold(Vec::new(), |mut relations, relation| {
                    if !relations.iter().any(|held| held == &relation) {
                        relations.push(relation);
                    }
                    relations
                })
                .iter()
                .flat_map(|relation| self.columns_of_except(relation, scope, wanted)),
        );

        // A keyword the dialect also has as a built-in (`NULL`, `COALESCE`) is
        // offered once, as the built-in, which knows how it is written.
        let builtins = self.engine.builtins();
        candidates.extend(
            KEYWORDS
                .iter()
                .filter(|keyword| {
                    !builtins
                        .iter()
                        .any(|builtin| builtin.name.eq_ignore_ascii_case(keyword))
                })
                .map(|keyword| Candidate::new(*keyword, None, CompletionItemKind::KEYWORD)),
        );
        candidates.extend(self.all_relations(qualify));
        candidates.extend(self.all_routines());
        // Last, so on a tie the user's own objects win and a dialect's long
        // list can't push them past the cap. Not after an unreadable
        // qualifier: `"o".date_trunc(` is never a call.
        if !before.ends_with('.') {
            candidates.extend(builtins.iter().map(Candidate::builtin));
        }
        candidates
    }

    /// [`Self::candidates`] for a MongoDB buffer: what `mql::completing` says
    /// the word is, from the catalog and the field names sampled for the
    /// collection's structure.
    fn mongo_candidates(
        &self,
        text: &str,
        word: Range<usize>,
        scope: Option<&(String, String)>,
        wanted: &mut Wanted,
    ) -> Vec<Candidate> {
        fn methods(names: impl Iterator<Item = &'static str>) -> Vec<Candidate> {
            names
                .map(|name| Candidate::new(name, None, CompletionItemKind::METHOD))
                .collect()
        }
        match mql::completing(text, word.start) {
            None => Vec::new(),
            // `db.` already names the database a collection is in.
            Some(Completing::Database) => {
                let mut candidates = self.all_relations(false);
                candidates.extend(methods(mql::DbMethod::ALL.iter().map(|m| m.name())));
                candidates
            }
            Some(Completing::Collection) => methods(mql::Method::ALL.iter().map(|m| m.name())),
            Some(Completing::Cursor) => methods(mql::CursorMethod::ALL.iter().map(|m| m.name())),
            // As in SQL, nothing typed is not a question.
            Some(Completing::Document { .. }) if word.is_empty() => Vec::new(),
            Some(Completing::Document { collection }) => {
                let mut candidates = match scope {
                    Some(scope) => self.columns_in(scope, wanted),
                    None => collection
                        .map(|collection| self.columns_of(&collection, wanted))
                        .unwrap_or_default(),
                };
                candidates.extend(
                    mql::OPERATORS.iter().map(|operator| {
                        Candidate::new(*operator, None, CompletionItemKind::OPERATOR)
                    }),
                );
                candidates
            }
        }
    }

    /// The whole decision, from buffer text to the rows the popup shows.
    ///
    /// Separate from [`CompletionProvider::completions`] and taking `&str`
    /// rather than a `Rope` so the tests can reach it without a window.
    /// Returns the rows to show, and the relations whose columns are wanted and
    /// not yet held — which the caller fetches, because this one is `&self` and
    /// has no window to spawn from.
    fn items(&self, sql: &str, offset: usize) -> (Vec<(Range<usize>, Candidate)>, Wanted) {
        self.items_in(sql, offset, None)
    }

    /// [`Self::items`] for text about one relation it does not name: its
    /// `(schema, relation)` is `scope`, whose columns are offered first.
    fn items_in(
        &self,
        sql: &str,
        offset: usize,
        scope: Option<&(String, String)>,
    ) -> (Vec<(Range<usize>, Candidate)>, Wanted) {
        let mut wanted = Vec::new();
        let offset = offset.min(sql.len());
        if !sql.is_char_boundary(offset) {
            return (Vec::new(), wanted);
        }

        let word = word_before(sql, offset);
        match self.engine.syntax() {
            Syntax::Sql => {}
            Syntax::Redis => {
                let (word, candidates) = redis_candidates(sql, offset);
                let ranked = rank(candidates, &sql[word.clone()], &mut self.fuzzy.borrow_mut())
                    .into_iter()
                    .map(|candidate| (word.clone(), candidate))
                    .collect();
                return (ranked, wanted);
            }
            Syntax::Mongo => {
                let candidates = self.mongo_candidates(sql, word.clone(), scope, &mut wanted);
                let ranked = rank(candidates, &sql[word.clone()], &mut self.fuzzy.borrow_mut())
                    .into_iter()
                    .map(|candidate| (word.clone(), candidate))
                    .collect();
                return (ranked, wanted);
            }
        }
        if suppressed(sql, word.start) {
            return (Vec::new(), wanted);
        }

        let prefix = &sql[word.clone()];
        let qualifier = qualifier_before(sql, word.start);

        // Nothing typed and nothing to qualify is not a question. Opening the
        // whole catalog on every space would put a popup over the buffer for
        // most of the time anyone is writing in it.
        if prefix.is_empty() && qualifier.is_none() {
            return (Vec::new(), wanted);
        }

        let statement = sql::Buffer::parse(sql)
            .statement_at(offset)
            .filter(|range| range.contains(&word.start) || range.end >= word.start)
            .unwrap_or(0..sql.len());
        let around = Around {
            statement: &sql[statement.clone()],
            before: &sql[..word.start],
            head: &sql[statement.start.min(word.start)..word.start],
            tail: &sql[offset..statement.end.max(offset)],
        };

        let candidates = self.candidates(&around, qualifier, scope, &mut wanted);
        let ranked = rank(candidates, prefix, &mut self.fuzzy.borrow_mut())
            .into_iter()
            .map(|candidate| (word.clone(), candidate))
            .collect();
        (ranked, wanted)
    }
}

impl CompletionProvider for SchemaCompletions {
    /// Every edit asks, and [`SchemaCompletions::items`] decides.
    ///
    /// Answering `false` here would leave an open popup standing, because the
    /// input only reconsiders the menu when the provider is consulted. An
    /// empty answer from `completions` hides it, so there is one place that
    /// decides whether a popup belongs and it is the one that knows.
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }

    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        // ponytail: the whole buffer is copied out of the rope on every
        // keystroke, and parsed once more on top of the highlighter's own
        // parse. A query buffer is a screen or two of text, so this is free at
        // the size it runs at -- hold the tokens on the provider and invalidate
        // them from `InputEvent::Change` if a very large buffer ever stutters.
        let sql = text.to_string();
        let (items, wanted) = self.items(&sql, offset);
        self.respond(text, items, wanted, cx)
    }
}

impl SchemaCompletions {
    /// As many of `wanted`, in order, as may be asked for alongside the
    /// fetches already in flight.
    fn admitted(&self, mut wanted: Wanted) -> Wanted {
        let in_flight = self
            .columns
            .borrow()
            .values()
            .filter(|state| matches!(state, ColumnState::Loading(_)))
            .count();
        wanted.truncate(FETCHES_IN_FLIGHT.saturating_sub(in_flight));
        wanted
    }

    /// Ask for the columns `items` wanted, and hand its rows to the popup as
    /// edits over `text`, the rope the ranges are offsets into.
    fn respond(
        &self,
        text: &Rope,
        items: Vec<(Range<usize>, Candidate)>,
        wanted: Wanted,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        // Marked before the request so a second keystroke arriving while the
        // first fetch is in flight does not ask again.
        for key in self.admitted(wanted) {
            let before = self.columns.borrow().get(&key).cloned();
            let attempts = match before {
                Some(ColumnState::Failed(attempts)) => attempts,
                _ => 0,
            };
            self.columns
                .borrow_mut()
                .insert(key.clone(), ColumnState::Loading(attempts));
            let asked = self.workspace.as_ref().is_some_and(|workspace| {
                let (schema, relation) = key.clone();
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.load_completion_columns(schema, relation, cx)
                    })
                    .unwrap_or(false)
            });
            // A `Loading` nothing will answer would hold a place in flight
            // for the rest of the session.
            if !asked {
                let mut columns = self.columns.borrow_mut();
                match before {
                    Some(state) => columns.insert(key, state),
                    None => columns.remove(&key),
                };
            }
        }

        // The popup does not reopen by itself when a fetch lands: the input
        // reconsiders the menu on an edit and nothing else. The next keystroke
        // shows the columns, which for a name being typed is the very next one.
        let items = items
            .into_iter()
            .map(|(word, candidate)| CompletionItem {
                // The edit is given explicitly rather than left to the input's
                // own guess at where the word started. That guess is taken on
                // the first keystroke of a run and not revised, so a word typed
                // one character at a time replaces the wrong span -- and the
                // wrong span here is a corrupted statement.
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: lsp_types::Range {
                        start: text.offset_to_position(word.start),
                        end: text.offset_to_position(word.end),
                    },
                    new_text: candidate.written(self.engine),
                })),
                documentation: candidate.documentation(),
                label: format!("{}{}", candidate.label, candidate.aliasing()),
                detail: candidate.detail,
                kind: Some(candidate.kind),
                ..Default::default()
            })
            .collect::<Vec<_>>();

        Task::ready(Ok(CompletionResponse::Array(items)))
    }
}

/// The completion source for a raw filter bar on one relation's tab.
///
/// A raw filter is only a `WHERE`, so on its own it names no relation and
/// [`SchemaCompletions`] would have no columns to offer. The tab's relation is
/// handed over as the scope instead, and everything else -- the column fetch,
/// the keywords, the silence inside a literal -- is the query editor's own.
///
/// The relation is never written into the text the scan reads: a name holding
/// a `'`, a `--` or a space would read as a literal, a comment or two names,
/// and silence the completion it was there to feed.
pub struct FilterCompletions {
    schema: Rc<SchemaCompletions>,
    relation: (String, String),
    /// What the text is read behind. A MongoDB filter is a document, which
    /// the scan only reads as one inside a call.
    prefix: &'static str,
}

impl FilterCompletions {
    /// `None` on an engine whose raw filter the schema cannot complete: Redis's
    /// is a `MATCH` glob over key names.
    pub fn new(
        syntax: Syntax,
        schema: Rc<SchemaCompletions>,
        relation: (String, String),
    ) -> Option<Self> {
        let prefix = match syntax {
            Syntax::Sql => "",
            // As `mql::browse::raw_filter` wraps it.
            Syntax::Mongo => "db.c.find(\n",
            Syntax::Redis => return None,
        };
        Some(Self {
            schema,
            relation,
            prefix,
        })
    }

    /// [`SchemaCompletions::items`] for the bar's own text, with the ranges
    /// back in that text.
    fn items(&self, text: &str, offset: usize) -> (Vec<(Range<usize>, Candidate)>, Wanted) {
        let shift = self.prefix.len();
        let (items, wanted) = self.schema.items_in(
            &format!("{}{text}", self.prefix),
            shift + offset,
            Some(&self.relation),
        );
        let items = items
            .into_iter()
            .filter(|(word, _)| word.start >= shift)
            // Qualifying is an editor setting, and the bar is not the editor.
            .map(|(word, candidate)| {
                let candidate = Candidate {
                    qualified: false,
                    ..candidate
                };
                (word.start - shift..word.end - shift, candidate)
            })
            .collect();
        (items, wanted)
    }
}

impl CompletionProvider for FilterCompletions {
    /// For the reason [`SchemaCompletions`] gives.
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }

    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let (items, wanted) = self.items(&text.to_string(), offset);
        self.schema.respond(text, items, wanted, cx)
    }
}

fn relation_kind(kind: RelationKind) -> CompletionItemKind {
    match kind {
        RelationKind::Table
        | RelationKind::PartitionedTable
        | RelationKind::ForeignTable
        | RelationKind::Dictionary => CompletionItemKind::STRUCT,
        RelationKind::View | RelationKind::MaterializedView => CompletionItemKind::INTERFACE,
    }
}

/// Score every candidate against what has been typed, best first.
///
/// The same matcher the palette uses, for the same reason: a fuzzy match is
/// what people expect from a list that narrows as they type.
///
/// Not [`Fuzzy::rank`]: that gives a tie to the shorter label, and here a tie
/// keeps the order [`SchemaCompletions::candidates`] built, which puts the
/// statement's own columns ahead of keywords.
fn rank(candidates: Vec<Candidate>, prefix: &str, fuzzy: &mut Fuzzy) -> Vec<Candidate> {
    let query = Query::new(prefix);
    let mut scored = candidates
        .into_iter()
        .filter_map(|candidate| Some((fuzzy.score(&query, &candidate.label)?, candidate)))
        .collect::<Vec<_>>();
    // A join the schema declares leads whatever its score: it has matched
    // what was typed like every other row, and there are only ever a few.
    scored.sort_by_key(|(score, candidate)| std::cmp::Reverse((candidate.on.is_some(), *score)));
    scored.truncate(MAX_ITEMS);
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// The command name being typed, when the caret is in the first word of its
/// line, and every name to rank against it. A name may hold a dot
/// (`FT.CREATE`), which `word_before` would cut at.
fn redis_candidates(text: &str, offset: usize) -> (Range<usize>, Vec<Candidate>) {
    let line = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
    let typed = &text[line..offset];
    let start = line + typed.len() - typed.trim_start().len();
    let word = start..offset;
    let first_word = !text[word.clone()].contains(char::is_whitespace);
    if !first_word || text[word.clone()].starts_with('#') || word.is_empty() {
        return (word, Vec::new());
    }
    let candidates = redis_cmd::names()
        .map(|(name, tag)| {
            Candidate::new(name, tag.map(String::from), CompletionItemKind::FUNCTION)
        })
        .collect();
    (word, candidates)
}

/// The identifier being typed, as a range ending at the cursor. Empty when the
/// cursor does not follow one.
///
/// Byte-wise, and safe to be: every byte of a multi-byte character is `>= 0x80`
/// and no ASCII test matches one, so a scan can never stop mid-character.
fn word_before(sql: &str, offset: usize) -> Range<usize> {
    let bytes = sql.as_bytes();
    let mut start = offset;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    start..offset
}

/// The identifier before the `.` that precedes the word being typed — the `o`
/// of `o.`, the `public` of `public.acc`.
fn qualifier_before(sql: &str, word_start: usize) -> Option<&str> {
    let bytes = sql.as_bytes();
    if word_start == 0 || bytes[word_start - 1] != b'.' {
        return None;
    }

    let dot = word_start - 1;
    let mut start = dot;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    (start < dot).then(|| &sql[start..dot])
}

/// Whether `at` falls inside a string literal or a comment.
///
/// The one thing a keyword scan cannot get right by itself, and the one place
/// a wrong completion does real damage: accepting a row inside a literal
/// rewrites data rather than a query.
///
/// ponytail: `'` for strings, `--` and the block form for comments, and
/// nothing else. Double quotes are deliberately absent -- Postgres reads them
/// as a quoted identifier, where completing is exactly right, and MySQL reads
/// them as a string, where it is not, and no scan can tell which server is
/// listening. Dollar-quoted bodies are absent for a smaller reason: the text
/// inside one is SQL too. Teach it the engine if a MySQL user is ever bitten.
fn suppressed(sql: &str, at: usize) -> bool {
    let bytes = sql.as_bytes();
    let at = at.min(bytes.len());
    let mut index = 0;

    while index < at {
        match bytes[index] {
            b'\'' => {
                index += 1;
                loop {
                    if index >= at || index >= bytes.len() {
                        return true;
                    }
                    if bytes[index] == b'\'' {
                        // Doubled, so it is an escaped quote and not the end.
                        if bytes.get(index + 1) == Some(&b'\'') {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                let end = sql[index..]
                    .find('\n')
                    .map_or(bytes.len(), |offset| index + offset);
                if at <= end {
                    return true;
                }
                index = end + 1;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let end = sql[index + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |offset| index + 2 + offset + 2);
                if at < end {
                    return true;
                }
                index = end;
            }
            _ => index += 1,
        }
    }

    false
}

/// The word-shaped tokens of a statement, in order. Dots are kept inside a
/// token so `public.accounts` stays one name rather than becoming two.
fn tokens(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if is_ident_byte(bytes[index]) || bytes[index] == b'.' {
            let start = index;
            while index < bytes.len() && (is_ident_byte(bytes[index]) || bytes[index] == b'.') {
                index += 1;
            }
            tokens.push(&sql[start..index]);
        } else {
            index += 1;
        }
    }

    tokens
}

fn is_keyword(token: &str) -> bool {
    KEYWORDS.iter().any(|keyword| {
        keyword
            .split(' ')
            .any(|word| word.eq_ignore_ascii_case(token))
    }) || SOURCE_KEYWORDS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(token))
}

/// The text around the word being completed.
#[derive(Clone, Copy)]
struct Around<'a> {
    /// The statement the caret is in, whole.
    statement: &'a str,
    /// The buffer up to the word.
    before: &'a str,
    /// The statement up to the word, and from the caret on.
    head: &'a str,
    tail: &'a str,
}

impl Around<'_> {
    /// The keyword the name being typed follows, past a schema typed in
    /// front of it.
    fn source(&self) -> Option<&str> {
        let tokens = tokens(self.before);
        let name = tokens.len().checked_sub(1)?;
        match tokens[name].ends_with('.') {
            true => tokens.get(name.checked_sub(1)?).copied(),
            false => Some(tokens[name]),
        }
    }

    /// Whether a relation written here can carry an alias: after `FROM` or
    /// `JOIN`, with nothing behind the caret that already is one.
    ///
    /// Not `DELETE FROM`, where SQLite and SQL Server take no alias, nor the
    /// other source keywords, where none does.
    fn takes_alias(&self) -> bool {
        let tokens = tokens(self.before);
        let after_delete = tokens
            .iter()
            .rev()
            .skip_while(|token| token.ends_with('.'))
            .nth(1)
            .is_some_and(|token| token.eq_ignore_ascii_case("delete"));
        let follows = self.source().is_some_and(|keyword| {
            ["from", "join"]
                .iter()
                .any(|k| keyword.eq_ignore_ascii_case(k))
        });
        follows && !after_delete && self.next().is_none_or(|next| next != Next::Name)
    }

    /// Whether a join condition can be written here as well: after `JOIN`,
    /// in front of no `ON` of the statement's own. Not after `CROSS JOIN` or
    /// `NATURAL JOIN`, which take none.
    fn takes_condition(&self) -> bool {
        let unconditional = tokens(self.before)
            .iter()
            .rev()
            .skip_while(|token| token.ends_with('.'))
            .skip(1)
            .take_while(|token| is_join_modifier(token))
            .any(|token| {
                ["cross", "natural"]
                    .iter()
                    .any(|k| token.eq_ignore_ascii_case(k))
            });
        self.source()
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("join"))
            && !unconditional
            && self.next().is_none_or(|next| next != Next::Condition)
    }

    /// What the text behind the caret opens with, when it is anything.
    fn next(&self) -> Option<Next> {
        let rest = self.tail.trim_start();
        let first = *rest.as_bytes().first()?;
        // The caret is inside a word, or in front of a quoted name or a
        // call's arguments: whatever is written here is not the whole name.
        let inside = rest.len() == self.tail.len() && is_ident_byte(first);
        // Nor is it known past a comment, which may sit in front of an alias.
        let comment = rest.starts_with("--") || rest.starts_with("/*");
        if inside || comment || matches!(first, b'"' | b'`' | b'[' | b'(' | b'.') {
            return Some(Next::Name);
        }
        if !is_ident_byte(first) {
            return Some(Next::Other);
        }
        let token = tokens(rest).into_iter().next()?;
        Some(match token {
            token
                if ["on", "using"]
                    .iter()
                    .any(|k| token.eq_ignore_ascii_case(k)) =>
            {
                Next::Condition
            }
            token if is_keyword(token) && !token.eq_ignore_ascii_case("as") => Next::Other,
            _ => Next::Name,
        })
    }
}

/// A word that may stand between a join's kind and its `JOIN`.
fn is_join_modifier(token: &str) -> bool {
    [
        "cross", "natural", "inner", "left", "right", "full", "outer",
    ]
    .iter()
    .any(|modifier| token.eq_ignore_ascii_case(modifier))
}

/// The part of `head` whose relations a condition at its end can name: less
/// every parenthesis already closed, and from the one the caret is inside.
/// A subquery's relations, or a CTE's, are out of reach of the query around
/// it, and the reverse.
fn reach(head: &str) -> String {
    let mut reach = String::new();
    let mut opened = Vec::new();
    for character in head.chars() {
        match character {
            '(' => opened.push(reach.len()),
            ')' => reach.truncate(opened.pop().unwrap_or(0)),
            _ => {}
        }
        // A space where a group closed, so the words around it stay two.
        reach.push(if character == ')' { ' ' } else { character });
    }
    match opened.last() {
        Some(&open) => reach.split_off(open),
        None => reach,
    }
}

/// What follows the caret, as far as an alias written at it is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    /// More of the name, or an alias the statement already gives it.
    Name,
    /// The join's own `ON` or `USING`.
    Condition,
    Other,
}

/// A relation a foreign key joins to one the statement holds.
#[derive(PartialEq, Eq)]
struct Join {
    /// Its schema and name, as the catalog spells them.
    target: (String, String),
    /// The name the statement knows the held relation by.
    onto: String,
    /// Its column and the held relation's, for each column of the key.
    columns: Vec<(String, String)>,
}

/// A relation's foreign keys, by the relation each points at, as that
/// relation's column and this one's.
///
/// ponytail: a [`ForeignKey`] is one column of a key and does not say which
/// key, so two keys onto one relation and one key of two columns arrive
/// looking alike. No key names a column twice, so a column met again opens
/// the next key: `created_by` and `updated_by` both onto `users.id` are two
/// joins. That leans on a key's columns arriving together, and reads two
/// single-column keys onto different columns of one relation as one key of
/// two -- carry the constraint's name on `ForeignKey` if either is ever met.
fn foreign_keys(keys: &[ForeignKey]) -> Vec<(JoinTarget, Vec<(String, String)>)> {
    let mut grouped: Vec<(JoinTarget, Vec<(String, String)>)> = Vec::new();
    for key in keys {
        let target = (key.referenced_schema.clone(), key.referenced_table.clone());
        let columns = (key.referenced_column.clone(), key.column.clone());
        match grouped.iter_mut().find(|(held, _)| held == &target) {
            Some((_, held)) => held.push(columns),
            None => grouped.push((target, vec![columns])),
        }
    }
    grouped
        .into_iter()
        .flat_map(|(target, columns)| {
            let mut keys: Vec<Vec<(String, String)>> = Vec::new();
            for (theirs, ours) in columns {
                let repeats = keys.last().is_none_or(|key| {
                    key.iter().any(|(held_theirs, held_ours)| {
                        held_theirs == &theirs || held_ours == &ours
                    })
                });
                match keys.last_mut().filter(|_| !repeats) {
                    Some(key) => key.push((theirs, ours)),
                    None => keys.push(vec![(theirs, ours)]),
                }
            }
            keys.into_iter().map(move |key| (target.clone(), key))
        })
        .collect()
}

/// The schema and relation a foreign key points at, as the key spells them.
type JoinTarget = (String, String);

/// Words an alias cannot be without quoting, beyond the keywords completion
/// offers. Short ones only: an alias is a relation's initials.
const RESERVED_ALIASES: &[&str] = &[
    "add", "all", "any", "at", "do", "for", "go", "if", "int", "key", "no", "of", "on", "or",
    "row", "set", "to", "top", "use",
];

/// The alias a relation is given: the initials of the words in its name,
/// numbered when the statement already uses them or they spell a keyword.
fn alias_for(relation: &str, taken: &[String]) -> String {
    let mut initials = String::new();
    let mut previous: Option<char> = None;
    for character in relation.chars() {
        let starts_word = previous.is_none_or(|previous| {
            !previous.is_ascii_alphanumeric()
                || (previous.is_ascii_lowercase() && character.is_ascii_uppercase())
        });
        if starts_word && character.is_ascii_alphanumeric() {
            initials.push(character.to_ascii_lowercase());
        }
        previous = Some(character);
    }
    // A name with no letters to take, or one whose first is a digit, which
    // cannot open an identifier.
    if !initials.starts_with(|first: char| first.is_ascii_lowercase()) {
        initials.insert(0, 't');
    }

    let free = |alias: &str| {
        !is_keyword(alias)
            && !RESERVED_ALIASES.contains(&alias)
            && !taken.iter().any(|held| held.eq_ignore_ascii_case(alias))
    };
    if free(&initials) {
        return initials;
    }
    (2..)
        .map(|number| format!("{initials}{number}"))
        .find(|alias| free(alias))
        .unwrap_or(initials)
}

/// Whether the caret sits where a relation name belongs — straight after
/// `FROM`, `JOIN`, `UPDATE`, `INSERT INTO` or `TABLE`.
fn expects_relation(before: &str) -> bool {
    tokens(before).last().is_some_and(|last| {
        SOURCE_KEYWORDS
            .iter()
            .any(|keyword| last.eq_ignore_ascii_case(keyword))
    })
}

/// Every name this statement binds to a relation: the relation's own name, and
/// its alias where it has one.
///
/// Returned as pairs rather than a map because the same alias can legitimately
/// be bound twice in one buffer and the first binding is the one in scope.
fn bindings(statement: &str) -> Vec<(String, String)> {
    sources(statement)
        .into_iter()
        .flat_map(|source| {
            let alias = source.alias.map(|alias| (alias, source.relation.clone()));
            std::iter::once((source.relation.clone(), source.relation)).chain(alias)
        })
        .collect()
}

/// A relation a statement reads from or writes to, as it names it.
struct Source {
    schema: Option<String>,
    relation: String,
    alias: Option<String>,
}

/// Every relation named after a source keyword, in order.
fn sources(statement: &str) -> Vec<Source> {
    let tokens = tokens(statement);
    let mut sources = Vec::new();
    let mut index = 0;

    while index < tokens.len() {
        let is_source = SOURCE_KEYWORDS
            .iter()
            .any(|keyword| tokens[index].eq_ignore_ascii_case(keyword));

        let Some(name) = tokens.get(index + 1).filter(|_| is_source) else {
            index += 1;
            continue;
        };
        if is_keyword(name) {
            index += 1;
            continue;
        }

        // A schema-qualified name binds under its last segment: `FROM
        // public.accounts` puts `accounts.` in reach, which is what anyone
        // types next.
        let mut segments = name.rsplit('.');
        let relation = segments.next().unwrap_or(name).to_string();
        let schema = segments.next().map(str::to_string);

        let alias = if tokens
            .get(index + 2)
            .is_some_and(|token| token.eq_ignore_ascii_case("as"))
        {
            tokens.get(index + 3)
        } else {
            tokens.get(index + 2)
        };
        sources.push(Source {
            schema,
            relation,
            alias: alias
                .filter(|alias| !is_keyword(alias))
                .map(|alias| (*alias).to_string()),
        });

        index += 2;
    }

    sources
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Engine, Relation, Routine, RoutineKind, Schema};

    /// A provider whose cache already holds every relation's columns, as it
    /// would once the statement had named them and the fetches had landed.
    ///
    /// `workspace: None` — these drive [`SchemaCompletions::items`], which
    /// reports what it wants rather than asking for it. What is *not* held is
    /// tested separately, in the two tests that build their own cache.
    fn detached(catalog: Catalog, columns: &[(&str, &str, &[&str])]) -> SchemaCompletions {
        let held = columns
            .iter()
            .map(|(schema, relation, columns)| {
                (
                    (schema.to_string(), relation.to_string()),
                    ColumnState::Loaded(Described {
                        columns: columns.iter().map(|column| column.to_string()).collect(),
                        ..Described::default()
                    }),
                )
            })
            .collect();
        SchemaCompletions {
            engine: Engine::Postgres,
            catalog: Arc::new(catalog),
            columns: Rc::new(RefCell::new(held)),
            qualify: false,
            alias: false,
            workspace: None,
            fuzzy: RefCell::new(Fuzzy::new()),
        }
    }

    fn relation(name: &str) -> Relation {
        Relation {
            name: name.to_string(),
            kind: RelationKind::Table,
            partition_of: None,
            size: None,
            rows: None,
        }
    }

    const COLUMNS: &[(&str, &str, &[&str])] = &[
        ("public", "accounts", &["id", "email", "created_at"]),
        ("public", "orders", &["id", "account_id", "total"]),
        ("audit", "events", &["id", "payload"]),
    ];

    fn schemas() -> Catalog {
        Catalog {
            schemas: vec![
                Schema {
                    name: "public".to_string(),
                    relations: vec![relation("accounts"), relation("orders")],
                    routines: vec![Routine {
                        name: "recalculate_totals".to_string(),
                        kind: RoutineKind::Procedure,
                        identity_arguments: String::new(),
                        result_type: String::new(),
                        language: "plpgsql".to_string(),
                        definition: String::new(),
                    }],
                },
                Schema {
                    name: "audit".to_string(),
                    relations: vec![relation("events")],
                    routines: Vec::new(),
                },
            ],
            database: None,
        }
    }

    fn catalog() -> SchemaCompletions {
        detached(schemas(), COLUMNS)
    }

    /// The labels offered for a caret at `|`.
    fn labels(sql: &str) -> Vec<String> {
        labels_on(Engine::Postgres, sql)
    }

    /// [`labels`] on another engine's dialect.
    fn labels_on(engine: Engine, sql: &str) -> Vec<String> {
        let offset = sql.find('|').expect("the test must mark the caret");
        let sql = sql.replace('|', "");
        SchemaCompletions {
            engine,
            ..catalog()
        }
        .items(&sql, offset)
        .0
        .into_iter()
        .map(|(_, candidate)| candidate.label)
        .collect()
    }

    #[test]
    fn after_from_it_offers_relations() {
        let offered = labels("SELECT * FROM acc|");
        assert_eq!(offered.first().map(String::as_str), Some("accounts"));
    }

    #[test]
    fn after_from_it_offers_no_keywords() {
        let offered = labels("SELECT * FROM a|");
        assert!(
            !offered.iter().any(|label| label == "AND"),
            "a relation position offered a keyword: {offered:?}"
        );
    }

    #[test]
    fn it_offers_schemas_where_a_relation_belongs() {
        assert!(labels("SELECT * FROM aud|").contains(&"audit".to_string()));
    }

    #[test]
    fn a_schema_qualifier_offers_its_relations() {
        assert_eq!(labels("SELECT * FROM audit.|"), vec!["events".to_string()]);
    }

    #[test]
    fn a_relation_qualifier_offers_its_columns() {
        assert_eq!(
            labels("SELECT accounts.| FROM accounts"),
            vec![
                "id".to_string(),
                "email".to_string(),
                "created_at".to_string()
            ]
        );
    }

    #[test]
    fn an_alias_offers_the_relations_columns() {
        assert_eq!(
            labels("SELECT o.| FROM orders o"),
            vec![
                "id".to_string(),
                "account_id".to_string(),
                "total".to_string()
            ]
        );
    }

    #[test]
    fn an_as_alias_is_read_the_same_way() {
        assert_eq!(
            labels("SELECT o.| FROM orders AS o"),
            vec![
                "id".to_string(),
                "account_id".to_string(),
                "total".to_string()
            ]
        );
    }

    #[test]
    fn an_alias_beats_a_relation_of_the_same_name() {
        // `orders o` makes `o` mean orders. Nothing else may claim it.
        assert!(labels("SELECT o.| FROM orders o").contains(&"total".to_string()));
    }

    #[test]
    fn an_unknown_qualifier_offers_nothing() {
        assert!(labels("SELECT nothing.| FROM accounts").is_empty());
    }

    #[test]
    fn columns_of_the_named_relation_come_before_keywords() {
        let offered = labels("SELECT e| FROM accounts");
        let email = offered.iter().position(|label| label == "email");
        let end = offered.iter().position(|label| label == "END");
        assert!(
            email < end,
            "a column of the statement's own relation ranked below a keyword: {offered:?}"
        );
    }

    #[test]
    fn a_schema_qualified_relation_still_binds_its_columns() {
        assert!(labels("SELECT accounts.| FROM public.accounts").contains(&"email".to_string()));
    }

    #[test]
    fn nothing_typed_and_nothing_qualified_offers_nothing() {
        assert!(labels("SELECT * FROM |").is_empty());
        assert!(labels("|").is_empty());
    }

    #[test]
    fn a_string_literal_suppresses_everything() {
        assert!(labels("SELECT * FROM accounts WHERE email = 'acc|'").is_empty());
    }

    #[test]
    fn a_closed_string_does_not_suppress_what_follows() {
        assert!(!labels("SELECT * FROM accounts WHERE email = 'x' AND ema|").is_empty());
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_literal() {
        assert!(labels("SELECT * FROM accounts WHERE email = 'it''s acc|'").is_empty());
    }

    #[test]
    fn a_line_comment_suppresses_to_the_end_of_the_line() {
        assert!(labels("-- pick the acc|\nSELECT 1").is_empty());
        assert!(!labels("-- a note\nSELECT * FROM acc|").is_empty());
    }

    #[test]
    fn a_block_comment_suppresses_until_it_closes() {
        assert!(labels("/* acc| */ SELECT 1").is_empty());
        assert!(!labels("/* a note */ SELECT * FROM acc|").is_empty());
    }

    #[test]
    fn the_edit_replaces_exactly_the_word_being_typed() {
        let sql = "SELECT * FROM acc";
        let (word, _) = catalog()
            .items(sql, sql.len())
            .0
            .into_iter()
            .next()
            .expect("a relation was offered");
        assert_eq!(&sql[word], "acc");
    }

    #[test]
    fn a_qualified_edit_replaces_the_word_and_not_the_qualifier() {
        let sql = "SELECT o.tot FROM orders o";
        let offset = sql.find(" FROM").expect("the caret sits after `tot`");
        let (word, candidate) = catalog()
            .items(sql, offset)
            .0
            .into_iter()
            .next()
            .expect("a column was offered");
        assert_eq!(&sql[word], "tot");
        assert_eq!(candidate.label, "total");
    }

    /// The first label offered for a caret at `|`, and what accepting it writes.
    fn first_edit(sql: &str, qualify: bool) -> (String, String) {
        let offset = sql.find('|').expect("the test must mark the caret");
        let sql = sql.replace('|', "");
        let completions = SchemaCompletions {
            qualify,
            ..catalog()
        };
        let (_, candidate) = completions
            .items(&sql, offset)
            .0
            .into_iter()
            .next()
            .expect("something was offered");
        (
            candidate.label.clone(),
            candidate.written(completions.engine),
        )
    }

    #[test]
    fn qualifying_writes_a_relation_with_its_schema_and_ranks_it_by_its_name() {
        assert_eq!(
            first_edit("SELECT * FROM eve|", true),
            ("events".to_string(), "audit.events".to_string())
        );
    }

    #[test]
    fn qualifying_does_not_repeat_a_schema_already_typed() {
        assert_eq!(
            first_edit("SELECT * FROM audit.eve|", true),
            ("events".to_string(), "events".to_string())
        );
    }

    #[test]
    fn qualifying_does_not_repeat_a_quoted_schema_already_typed() {
        assert_eq!(
            first_edit("SELECT * FROM \"audit\".eve|", true),
            ("events".to_string(), "events".to_string())
        );
    }

    #[test]
    fn a_qualified_name_quotes_each_part_that_would_not_read_back_unquoted() {
        let completions = SchemaCompletions {
            qualify: true,
            ..detached(
                Catalog {
                    schemas: vec![Schema {
                        name: "Audit Log".to_string(),
                        relations: vec![relation("events")],
                        routines: Vec::new(),
                    }],
                    database: None,
                },
                &[],
            )
        };
        let (_, candidate) = completions
            .items("SELECT * FROM eve", 17)
            .0
            .into_iter()
            .next()
            .expect("a relation was offered");
        assert_eq!(
            candidate.written(completions.engine),
            "\"Audit Log\".events"
        );
    }

    #[test]
    fn a_relation_is_written_bare_unless_qualifying_is_on() {
        assert_eq!(
            first_edit("SELECT * FROM eve|", false),
            ("events".to_string(), "events".to_string())
        );
    }

    #[test]
    fn a_routine_is_offered() {
        assert!(labels("SELECT recalc|").contains(&"recalculate_totals".to_string()));
    }

    #[test]
    fn a_dialects_own_built_ins_are_offered_on_its_engine_only() {
        assert!(labels("SELECT date_tr|").contains(&"date_trunc".to_string()));
        assert!(
            !labels_on(Engine::SqlServer, "SELECT date_tr|").contains(&"date_trunc".to_string())
        );
        assert!(labels_on(Engine::SqlServer, "SELECT getd|").contains(&"GETDATE".to_string()));
        assert!(!labels("SELECT getd|").contains(&"GETDATE".to_string()));
    }

    #[test]
    fn a_built_in_is_written_as_a_call_or_a_bare_value() {
        let written = |sql: &str, label: &str| {
            let completions = catalog();
            let (_, candidate) = completions
                .items(sql, sql.len())
                .0
                .into_iter()
                .find(|(_, candidate)| candidate.label == label)
                .expect("the built-in was offered");
            assert!(candidate.documentation().is_some(), "{label}");
            candidate.written(completions.engine)
        };
        assert_eq!(written("SELECT date_trunc", "date_trunc"), "date_trunc(");
        assert_eq!(written("SELECT now", "now"), "now()");
        assert_eq!(written("SELECT current_d", "CURRENT_DATE"), "CURRENT_DATE");
    }

    #[test]
    fn current_offers_the_current_values() {
        let offered = labels("SELECT CURRENT_|");
        for value in ["CURRENT_DATE", "CURRENT_TIMESTAMP", "CURRENT_USER"] {
            assert!(offered.contains(&value.to_string()), "{offered:?}");
        }
    }

    #[test]
    fn a_keyword_that_is_also_a_built_in_is_offered_once() {
        let offered = labels("SELECT * FROM accounts WHERE email = NUL|");
        assert_eq!(
            offered.iter().filter(|label| *label == "NULL").count(),
            1,
            "{offered:?}"
        );
        let sql = "SELECT * FROM accounts WHERE email = NUL";
        let (_, null) = catalog()
            .items(sql, sql.len())
            .0
            .into_iter()
            .find(|(_, candidate)| candidate.label == "NULL")
            .expect("NULL was offered");
        assert_eq!(null.kind, CompletionItemKind::CONSTANT);
    }

    #[test]
    fn no_built_in_is_offered_where_a_relation_belongs_or_inside_a_literal() {
        assert!(!labels("SELECT * FROM date_tr|").contains(&"date_trunc".to_string()));
        assert!(
            !labels(r#"SELECT "o".date_tr| FROM orders o"#).contains(&"date_trunc".to_string())
        );
        assert!(labels("SELECT 'now|'").is_empty());
    }

    #[test]
    fn a_raw_filter_offers_built_ins() {
        let offered = filter_items(Engine::Postgres, "accounts", "created_at < no|");
        assert!(
            offered.contains(&("no".to_string(), "now".to_string())),
            "{offered:?}"
        );
    }

    #[test]
    fn the_list_is_capped() {
        let wide = (0..500)
            .map(|index| format!("column_{index}"))
            .collect::<Vec<_>>();
        let names = wide.iter().map(String::as_str).collect::<Vec<_>>();
        let completions = detached(
            Catalog {
                schemas: vec![Schema {
                    name: "public".to_string(),
                    relations: vec![relation("wide")],
                    routines: Vec::new(),
                }],
                database: None,
            },
            &[("public", "wide", &names)],
        );
        let sql = "SELECT wide. FROM wide";
        let offset = sql.find(' ').unwrap() + "wide.".len() + 1;
        assert_eq!(completions.items(sql, offset).0.len(), MAX_ITEMS);
    }

    #[test]
    fn a_relation_whose_columns_are_not_held_yet_is_asked_for_once() {
        let completions = detached(schemas(), &[]);
        let sql = "SELECT accounts.";

        let (items, wanted) = completions.items(sql, sql.len());
        assert!(items.is_empty(), "nothing is known about its columns yet");
        assert_eq!(wanted, vec![("public".to_string(), "accounts".to_string())]);

        // What the provider does on a miss, so the second keystroke asks for
        // nothing while the first fetch is still out.
        completions.columns.borrow_mut().insert(
            ("public".to_string(), "accounts".to_string()),
            ColumnState::Loading(0),
        );
        assert!(completions.items(sql, sql.len()).1.is_empty());
    }

    #[test]
    fn a_keystroke_asks_for_no_more_relations_than_may_be_in_flight() {
        let many = Catalog {
            schemas: (0..200)
                .map(|n| Schema {
                    name: format!("tenant_{n}"),
                    relations: vec![relation("users")],
                    routines: Vec::new(),
                })
                .collect(),
            database: None,
        };
        let completions = detached(many, &[]);
        let sql = "SELECT * FROM users WHERE u";

        let (_, wanted) = completions.items(sql, sql.len());
        assert_eq!(wanted.len(), 200, "every schema's users is wanted");
        let first = completions.admitted(wanted.clone());
        assert_eq!(first, wanted[..FETCHES_IN_FLIGHT]);

        completions
            .columns
            .borrow_mut()
            .insert(first[0].clone(), ColumnState::Loading(0));
        assert_eq!(
            completions.admitted(wanted.clone()).len(),
            FETCHES_IN_FLIGHT - 1
        );
        for key in &first {
            completions
                .columns
                .borrow_mut()
                .insert(key.clone(), ColumnState::Loading(0));
        }
        assert!(completions.admitted(wanted).is_empty());
    }

    #[test]
    fn a_name_the_catalog_does_not_list_is_never_asked_for() {
        // Or a typo would put a describe on the wire for every keystroke it
        // stays on screen.
        let completions = detached(schemas(), &[]);
        let sql = "SELECT accunts.";
        assert!(completions.items(sql, sql.len()).1.is_empty());
    }

    #[test]
    fn a_relation_the_server_would_not_describe_is_not_asked_again() {
        let completions = detached(schemas(), &[]);
        completions.columns.borrow_mut().insert(
            ("public".to_string(), "accounts".to_string()),
            ColumnState::Failed(FETCH_ATTEMPTS),
        );
        let sql = "SELECT accounts.";
        assert!(completions.items(sql, sql.len()).1.is_empty());
    }

    #[test]
    fn a_failure_is_retried_until_the_budget_runs_out() {
        // One blip -- or one statement timeout, which bounds dbdelve's own
        // catalog queries too -- must not silently kill completion for a
        // relation for the rest of the connection.
        let completions = detached(schemas(), &[]);
        let key = ("public".to_string(), "accounts".to_string());
        let sql = "SELECT accounts.";

        for attempts in 0..FETCH_ATTEMPTS {
            completions
                .columns
                .borrow_mut()
                .insert(key.clone(), ColumnState::Failed(attempts));
            assert_eq!(
                completions.items(sql, sql.len()).1,
                vec![key.clone()],
                "a relation that has failed {attempts} times is still worth asking about"
            );
        }
    }

    #[test]
    fn only_the_relations_a_statement_names_are_asked_for() {
        // The whole point of the cache: writing about one table must not fetch
        // the columns of every table in the database.
        let completions = detached(schemas(), &[]);
        let sql = "SELECT e FROM audit.events";
        let (_, wanted) = completions.items(sql, sql.find(" FROM").unwrap());
        assert_eq!(wanted, vec![("audit".to_string(), "events".to_string())]);
    }

    /// The labels offered in a MongoDB buffer for a caret at `|`, over a
    /// catalog holding `users` (sampled: `_id`, `name`, `email`) and `orders`.
    fn mongo_labels(text: &str) -> Vec<String> {
        let offset = text.find('|').expect("the test must mark the caret");
        let text = text.replace('|', "");
        let mut completions = detached(
            Catalog {
                schemas: vec![Schema {
                    name: "shop".to_string(),
                    relations: vec![relation("users"), relation("orders")],
                    routines: Vec::new(),
                }],
                database: None,
            },
            &[("shop", "users", &["_id", "name", "email"])],
        );
        completions.engine = Engine::MongoDb;
        completions
            .items(&text, offset)
            .0
            .into_iter()
            .map(|(_, candidate)| candidate.label)
            .collect()
    }

    #[test]
    fn after_db_a_mongo_buffer_offers_collections_then_database_methods() {
        let offered = mongo_labels("db.|");
        assert_eq!(&offered[..2], ["users", "orders"]);
        assert!(offered.contains(&"runCommand".to_string()), "{offered:?}");
        assert_eq!(mongo_labels("db.ord|"), vec!["orders"]);
    }

    #[test]
    fn after_a_collection_its_methods_are_offered() {
        for text in [
            "db.users.|",
            "db.getCollection(\"users\").|",
            "db.getSiblingDB('other').users.|",
        ] {
            let offered = mongo_labels(text);
            assert!(
                offered.contains(&"insertOne".to_string()),
                "{text}: {offered:?}"
            );
            assert!(
                !offered.contains(&"limit".to_string()),
                "{text}: {offered:?}"
            );
        }
        assert_eq!(mongo_labels("db.users.countD|"), vec!["countDocuments"]);
    }

    #[test]
    fn after_find_the_cursor_methods_are_offered() {
        for text in [
            "db.users.find({ a: \")\" }).|",
            "db.users.find({}).sort({ name: 1 })\n  .|",
            "db.users.aggregate([]).|",
        ] {
            let offered = mongo_labels(text);
            assert!(
                offered.contains(&"limit".to_string()),
                "{text}: {offered:?}"
            );
            assert!(
                !offered.contains(&"insertOne".to_string()),
                "{text}: {offered:?}"
            );
        }
        // A cursor method is not offered after a method that returns none.
        assert!(mongo_labels("db.users.countDocuments({}).|").is_empty());
    }

    #[test]
    fn inside_a_document_fields_come_before_operators() {
        let offered = mongo_labels("db.users.find({ na|");
        assert_eq!(offered.first().map(String::as_str), Some("name"));
        assert!(mongo_labels("db.users.find({ age: { $g| } })").contains(&"$gte".to_string()));
        assert!(
            mongo_labels("db.getCollection('users').aggregate([{ $match: { em| } }])")
                .contains(&"email".to_string())
        );
        // No collection named, still operators.
        assert!(mongo_labels("db.runCommand({ $or| })").contains(&"$or".to_string()));
    }

    fn redis_labels(text: &str) -> Vec<String> {
        let offset = text.find('|').expect("the test must mark the caret");
        let text = text.replace('|', "");
        let mut completions = detached(
            Catalog {
                schemas: Vec::new(),
                database: None,
            },
            &[],
        );
        completions.engine = Engine::Redis;
        completions
            .items(&text, offset)
            .0
            .into_iter()
            .map(|(_, candidate)| candidate.label)
            .collect()
    }

    #[test]
    fn a_redis_buffer_offers_command_names_in_a_lines_first_word_only() {
        assert!(redis_labels("hge|").contains(&"HGETALL".to_string()));
        assert!(redis_labels("GET a\n  ft.cre|").contains(&"FT.CREATE".to_string()));
        assert!(redis_labels("GET ke|").is_empty());
        assert!(redis_labels("# ge|").is_empty());
        assert!(redis_labels("|").is_empty());
    }

    #[test]
    fn a_mongo_string_comment_or_regex_offers_nothing() {
        for text in [
            "db.users.find({ name: \"na|\" })",
            "db.users.find({ name: 'it\\'s na|' })",
            "// db.|",
            "db.users.find({ /* na| */ })",
            "db.users.find({ name: /na|/ })",
        ] {
            assert!(mongo_labels(text).is_empty(), "{text}");
        }
        assert!(!mongo_labels("db.users.find({ name: /x/ }).|").is_empty());
        assert!(!mongo_labels("// a note\ndb.|").is_empty());
    }

    /// What a raw filter on relation `name` offers for a caret at `|`: each label,
    /// with the text of the filter's own that accepting it would replace.
    ///
    /// Over a catalog whose relations are named the way a scan of SQL would
    /// misread, each with the same columns.
    fn filter_items(engine: Engine, name: &str, text: &str) -> Vec<(String, String)> {
        const AWKWARD: &[&str] = &["people's", "a--b", "has space", "say \"hi\"", "accounts"];
        let offset = text.find('|').expect("the test must mark the caret");
        let text = text.replace('|', "");
        let held = AWKWARD
            .iter()
            .map(|awkward| ("public", *awkward, &["id", "email"][..]))
            .collect::<Vec<_>>();
        let mut schema = detached(
            Catalog {
                schemas: vec![Schema {
                    name: "public".to_string(),
                    relations: AWKWARD.iter().map(|awkward| relation(awkward)).collect(),
                    routines: Vec::new(),
                }],
                database: None,
            },
            &held,
        );
        schema.engine = engine;
        let scope = ("public".to_string(), name.to_string());
        FilterCompletions::new(engine.syntax(), Rc::new(schema), scope)
            .expect("the engine completes a raw filter")
            .items(&text, offset)
            .0
            .into_iter()
            .map(|(word, candidate)| (text[word].to_string(), candidate.label))
            .collect()
    }

    #[test]
    fn a_raw_filter_offers_its_relations_columns_over_its_own_text() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::SqlServer] {
            for relation in ["accounts", "people's", "a--b", "has space", "say \"hi\""] {
                let offered = filter_items(engine, relation, "id > 5 AND ema|");
                assert_eq!(
                    offered.first(),
                    Some(&("ema".to_string(), "email".to_string())),
                    "{engine:?} {relation}"
                );
            }
        }
        // At the very start of the text.
        let offered = filter_items(Engine::Postgres, "people's", "ema|");
        assert_eq!(
            offered.first(),
            Some(&("ema".to_string(), "email".to_string()))
        );
    }

    #[test]
    fn a_raw_filter_naming_its_own_relation_offers_and_asks_for_it_once() {
        let catalog = Catalog {
            schemas: vec![
                Schema {
                    name: "public".to_string(),
                    relations: vec![relation("accounts")],
                    routines: Vec::new(),
                },
                Schema {
                    name: "audit".to_string(),
                    relations: vec![relation("accounts")],
                    routines: Vec::new(),
                },
            ],
            database: None,
        };
        let text = "parent_id IN (SELECT id FROM accounts) AND ema";
        let scope = ("public".to_string(), "accounts".to_string());
        let filter = |held: &[(&str, &str, &[&str])]| {
            FilterCompletions::new(
                Syntax::Sql,
                Rc::new(detached(catalog.clone(), held)),
                scope.clone(),
            )
            .expect("SQL completes a raw filter")
            .items(text, text.len())
        };

        let (items, _) = filter(&[
            ("public", "accounts", &["id", "email"]),
            ("audit", "accounts", &["id", "embargo"]),
        ]);
        let labels: Vec<_> = items
            .iter()
            .map(|(_, candidate)| candidate.label.as_str())
            .collect();
        assert_eq!(
            labels.iter().filter(|label| **label == "email").count(),
            1,
            "{labels:?}"
        );
        // Another schema's relation of the same name is still the text's.
        assert!(labels.contains(&"embargo"), "{labels:?}");

        let (_, wanted) = filter(&[]);
        let asked = wanted.iter().filter(|key| **key == scope).count();
        assert_eq!(asked, 1, "{wanted:?}");
    }

    #[test]
    fn a_raw_filter_offers_nothing_inside_a_string() {
        assert!(filter_items(Engine::Postgres, "accounts", "email = 'ema|").is_empty());
    }

    #[test]
    fn a_mongo_raw_filter_offers_the_collections_fields() {
        for collection in ["accounts", "people's", "say \"hi\""] {
            let offered = filter_items(Engine::MongoDb, collection, "{ ema|");
            assert_eq!(
                offered.first(),
                Some(&("ema".to_string(), "email".to_string())),
                "{collection}"
            );
        }
        assert!(filter_items(Engine::MongoDb, "accounts", "{ email: \"ema|\" }").is_empty());
    }

    #[test]
    fn a_redis_raw_filter_completes_nothing() {
        let scope = ("0".to_string(), "keys".to_string());
        assert!(FilterCompletions::new(Syntax::Redis, Rc::new(catalog()), scope).is_none());
    }

    #[test]
    fn a_second_statement_does_not_borrow_the_first_statements_relations() {
        // Each statement is its own scope: the `accounts` in the first one must
        // not put `email` under the caret in the second.
        let offered = labels("SELECT * FROM accounts;\nSELECT ema| FROM audit.events");
        assert!(
            !offered.contains(&"email".to_string()),
            "a column leaked across a statement boundary: {offered:?}"
        );
    }

    fn key(column: &str, table: &str, referenced: &str) -> ForeignKey {
        ForeignKey {
            column: column.to_string(),
            referenced_schema: "public".to_string(),
            referenced_table: table.to_string(),
            referenced_column: referenced.to_string(),
        }
    }

    /// A shop whose every relation has been described, foreign keys and all,
    /// with aliasing on.
    fn shop() -> SchemaCompletions {
        let reference = |table: &str, column: &str| Reference {
            schema: "public".to_string(),
            table: table.to_string(),
            column: column.to_string(),
            referenced_column: "id".to_string(),
        };
        let described: Vec<(&str, Vec<ForeignKey>, Vec<Reference>)> = vec![
            (
                "customers",
                vec![],
                vec![reference("orders", "customer_id")],
            ),
            (
                "orders",
                vec![key("customer_id", "customers", "id")],
                vec![reference("order_items", "order_id")],
            ),
            ("order_items", vec![key("order_id", "orders", "id")], vec![]),
            (
                "employees",
                vec![key("manager_id", "employees", "id")],
                vec![reference("employees", "manager_id")],
            ),
            (
                "posts",
                vec![
                    key("created_by", "users", "id"),
                    key("updated_by", "users", "id"),
                ],
                vec![],
            ),
            ("users", vec![], vec![]),
            (
                "shipments",
                vec![
                    key("order_id", "order_lines", "order_id"),
                    key("line_no", "order_lines", "line_no"),
                ],
                vec![],
            ),
            ("order_lines", vec![], vec![]),
            ("OrderItems", vec![], vec![]),
            ("is_new", vec![], vec![]),
        ];
        let catalog = Catalog {
            schemas: vec![Schema {
                name: "public".to_string(),
                relations: described.iter().map(|(name, ..)| relation(name)).collect(),
                routines: Vec::new(),
            }],
            database: None,
        };
        let held = described
            .into_iter()
            .map(|(name, foreign_keys, referenced_by)| {
                (
                    ("public".to_string(), name.to_string()),
                    ColumnState::Loaded(Described {
                        columns: vec!["id".to_string()],
                        foreign_keys,
                        referenced_by,
                    }),
                )
            })
            .collect();
        SchemaCompletions {
            columns: Rc::new(RefCell::new(held)),
            alias: true,
            ..detached(catalog, &[])
        }
    }

    /// What the popup shows and accepting writes, which are one text for an
    /// unqualified relation, for a caret at `|`.
    fn offered(completions: &SchemaCompletions, sql: &str) -> Vec<String> {
        let offset = sql.find('|').expect("the test must mark the caret");
        let sql = sql.replace('|', "");
        completions
            .items(&sql, offset)
            .0
            .into_iter()
            .map(|(_, candidate)| candidate.written(completions.engine))
            .collect()
    }

    fn first(sql: &str) -> String {
        offered(&shop(), sql).remove(0)
    }

    #[test]
    fn a_relation_after_from_is_written_with_its_initials() {
        assert_eq!(first("SELECT * FROM custo|"), "customers c");
        assert_eq!(first("SELECT * FROM order_it|"), "order_items oi");
        assert_eq!(first("SELECT * FROM OrderI|"), "OrderItems oi");
        assert_eq!(first("SELECT * FROM public.custo|"), "customers c");
    }

    #[test]
    fn an_alias_steps_around_the_statements_own_and_around_keywords() {
        assert_eq!(
            first("SELECT * FROM order_items oi JOIN OrderI|"),
            "OrderItems oi2"
        );
        assert_eq!(
            first("SELECT * FROM users o JOIN order_lin|"),
            "order_lines ol"
        );
        assert_eq!(first("SELECT * FROM users c JOIN custo|"), "customers c2");
        // `in` would not read back as a name.
        assert_eq!(first("SELECT * FROM is_n|"), "is_new in2");
        // The word being typed is not a relation the statement holds.
        assert_eq!(first("SELECT * FROM c|"), "customers c");
    }

    #[test]
    fn no_alias_is_written_where_none_belongs() {
        for sql in [
            "UPDATE custo|",
            "INSERT INTO custo|",
            "DELETE FROM custo|",
            "SELECT * FROM custo| c",
            "SELECT * FROM custo| AS c",
            "SELECT * FROM custo|mers",
        ] {
            assert_eq!(first(sql), "customers", "{sql}");
        }
        assert_eq!(first("SELECT * FROM custo| WHERE true"), "customers c");
        assert_eq!(
            first("SELECT * FROM (SELECT * FROM custo|) x"),
            "customers c"
        );

        let off = SchemaCompletions {
            alias: false,
            ..shop()
        };
        assert_eq!(
            offered(&off, "SELECT * FROM orders o JOIN custo|")[0],
            "customers"
        );
    }

    #[test]
    fn a_join_is_offered_with_the_condition_a_foreign_key_implies() {
        // The key is the joined relation's own, found from who points back.
        assert_eq!(
            offered(&shop(), "SELECT * FROM orders o JOIN order_it|")[..2],
            ["order_items oi ON oi.order_id = o.id", "order_items oi"]
        );
        // The key is the held relation's.
        assert_eq!(
            first("SELECT * FROM orders o JOIN cust|"),
            "customers c ON c.id = o.customer_id"
        );
        // A relation with no alias is named in full.
        assert_eq!(
            first("SELECT * FROM public.orders LEFT JOIN cust|"),
            "customers c ON c.id = orders.customer_id"
        );
        assert_eq!(
            first("SELECT * FROM orders o JOIN public.cust|"),
            "customers c ON c.id = o.customer_id"
        );
    }

    #[test]
    fn a_join_leads_a_relation_that_matches_better() {
        assert_eq!(
            offered(&shop(), "SELECT * FROM order_items oi JOIN o|")[0],
            "orders o ON o.id = oi.order_id"
        );
    }

    #[test]
    fn keys_onto_one_relation_are_told_apart_from_one_key_of_many_columns() {
        assert_eq!(
            offered(&shop(), "SELECT * FROM posts p JOIN us|")[..3],
            [
                "users u ON u.id = p.created_by",
                "users u ON u.id = p.updated_by",
                "users u"
            ]
        );
        assert_eq!(
            first("SELECT * FROM shipments s JOIN order_l|"),
            "order_lines ol ON ol.order_id = s.order_id AND ol.line_no = s.line_no"
        );
    }

    #[test]
    fn a_relation_already_held_is_joined_again_only_to_itself() {
        assert_eq!(
            offered(&shop(), "SELECT * FROM employees e JOIN emp|"),
            [
                "employees e2 ON e2.id = e.manager_id",
                "employees e2 ON e2.manager_id = e.id",
                "employees e2"
            ]
        );
        assert_eq!(
            offered(
                &shop(),
                "SELECT * FROM orders o JOIN order_items oi ON oi.order_id = o.id JOIN ord|"
            )[0],
            "orders o2"
        );
    }

    #[test]
    fn no_condition_is_written_in_front_of_the_statements_own() {
        assert_eq!(
            first("SELECT * FROM orders o JOIN cust| ON true"),
            "customers c"
        );
        assert_eq!(first("SELECT * FROM orders o, cust|"), "customers");
    }

    #[test]
    fn no_condition_follows_a_join_that_takes_none_or_names_a_relation_out_of_reach() {
        for sql in [
            "SELECT * FROM orders o CROSS JOIN cust|",
            "SELECT * FROM orders o NATURAL LEFT JOIN cust|",
            "SELECT * FROM orders o JOIN cust| /* keep */ ON true",
            "WITH x AS (SELECT * FROM orders o) SELECT * FROM users u JOIN cust|",
        ] {
            assert!(!first(sql).contains(" ON "), "{sql}");
        }
        assert_eq!(
            first("SELECT * FROM users u JOIN (SELECT * FROM orders o JOIN cust|"),
            "customers c ON c.id = o.customer_id"
        );
    }

    #[test]
    fn two_keys_of_two_columns_onto_one_relation_stay_whole() {
        let keys = [
            key("billing_tenant", "addresses", "tenant_id"),
            key("billing_id", "addresses", "id"),
            key("shipping_tenant", "addresses", "tenant_id"),
            key("shipping_id", "addresses", "id"),
        ];
        let columns = foreign_keys(&keys)
            .into_iter()
            .map(|(_, columns)| columns.len())
            .collect::<Vec<_>>();
        assert_eq!(columns, [2, 2]);
    }

    #[test]
    fn a_join_asks_for_the_keys_of_a_relation_not_yet_described() {
        let completions = SchemaCompletions {
            columns: ColumnCache::default(),
            ..shop()
        };
        let sql = "SELECT * FROM orders o JOIN cust";
        let (items, wanted) = completions.items(sql, sql.len());
        assert_eq!(wanted, [("public".to_string(), "orders".to_string())]);
        assert_eq!(items[0].1.written(Engine::Postgres), "customers c");
    }

    #[test]
    fn a_filter_bar_writes_no_alias() {
        let scope = ("public".to_string(), "orders".to_string());
        let bar = FilterCompletions::new(Syntax::Sql, Rc::new(shop()), scope).unwrap();
        let text = "id IN (SELECT id FROM cust";
        let (items, _) = bar.items(text, text.len());
        assert_eq!(items[0].1.written(Engine::Postgres), "customers");
    }
}
