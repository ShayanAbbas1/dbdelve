//! What the editor offers while you type.
//!
//! gpui-component's input already owns the whole completion surface — the
//! popup under the caret, its scroll, the arrow keys, `enter` to accept and
//! `escape` to dismiss — behind one trait with two required methods. None of
//! it needs a language server; `lsp_types` is only the vocabulary. So this
//! module is the part that is dbdelve's: which identifiers are worth offering
//! for the text in front of the cursor.
//!
//! Everything offered comes from the catalog the explorer already loaded, so
//! the popup can never name a relation the tree does not show. Nothing here
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
    TextEdit,
};

use crate::{
    Workspace,
    db::{Catalog, RelationKind, Syntax},
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
    Loaded(Vec<String>),
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

/// How many times a relation's columns are asked for before dbdelve stops.
const FETCH_ATTEMPTS: u8 = 3;

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
}

impl Candidate {
    fn new(label: impl Into<String>, detail: Option<String>, kind: CompletionItemKind) -> Self {
        Self {
            label: label.into(),
            detail,
            kind,
        }
    }
}

/// The completion source for a connected profile's editor.
///
/// Holds a snapshot rather than a live handle. The catalog is replaced whole
/// when it reloads, and so is this — see `Workspace::install_completions`.
pub struct SchemaCompletions {
    syntax: Syntax,
    catalog: Arc<Catalog>,
    columns: ColumnCache,
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
        syntax: Syntax,
        catalog: Arc<Catalog>,
        columns: ColumnCache,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        Self {
            syntax,
            catalog,
            columns,
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
        let mut want = |key| {
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        };
        match self.columns.borrow().get(&key) {
            Some(ColumnState::Loaded(columns)) => columns
                .iter()
                .map(|column| {
                    Candidate::new(column, Some(key.1.clone()), CompletionItemKind::FIELD)
                })
                .collect(),
            // Failed, but not yet often enough to give up on.
            Some(ColumnState::Failed(attempts)) if *attempts < FETCH_ATTEMPTS => {
                want(key);
                Vec::new()
            }
            // In flight, or asked for as often as it is going to be.
            Some(ColumnState::Loading(_) | ColumnState::Failed(_)) => Vec::new(),
            None => {
                want(key);
                Vec::new()
            }
        }
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

    fn all_relations(&self) -> Vec<Candidate> {
        self.catalog
            .schemas
            .iter()
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
        statement: &str,
        before: &str,
        qualifier: Option<&str>,
        scope: Option<&(String, String)>,
        wanted: &mut Vec<(String, String)>,
    ) -> Vec<Candidate> {
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
            return self.relations_in(qualifier);
        }

        if expects_relation(before) {
            let mut candidates = self.all_relations();
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

        candidates.extend(
            KEYWORDS
                .iter()
                .map(|keyword| Candidate::new(*keyword, None, CompletionItemKind::KEYWORD)),
        );
        candidates.extend(self.all_relations());
        candidates.extend(self.all_routines());
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
            Some(Completing::Database) => {
                let mut candidates = self.all_relations();
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
        match self.syntax {
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
            .map(|range| &sql[range])
            .unwrap_or(sql);
        let before = &sql[..word.start];

        let candidates = self.candidates(statement, before, qualifier, scope, &mut wanted);
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
        for key in wanted {
            let attempts = match self.columns.borrow().get(&key) {
                Some(ColumnState::Failed(attempts)) => *attempts,
                _ => 0,
            };
            self.columns
                .borrow_mut()
                .insert(key.clone(), ColumnState::Loading(attempts));
            if let Some(workspace) = &self.workspace {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.load_completion_columns(key.0, key.1, cx);
                    })
                    .ok();
            }
        }

        // The popup does not reopen by itself when a fetch lands: the input
        // reconsiders the menu on an edit and nothing else. The next keystroke
        // shows the columns, which for a name being typed is the very next one.
        let items = items
            .into_iter()
            .map(|(word, candidate)| CompletionItem {
                label: candidate.label.clone(),
                detail: candidate.detail,
                kind: Some(candidate.kind),
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
                    new_text: candidate.label,
                })),
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
            .map(|(word, candidate)| (word.start - shift..word.end - shift, candidate))
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
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
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
    let tokens = tokens(statement);
    let mut bindings = Vec::new();
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
        let relation = name.rsplit('.').next().unwrap_or(name).to_string();
        bindings.push((relation.clone(), relation.clone()));

        let alias = if tokens
            .get(index + 2)
            .is_some_and(|token| token.eq_ignore_ascii_case("as"))
        {
            tokens.get(index + 3)
        } else {
            tokens.get(index + 2)
        };
        if let Some(alias) = alias.filter(|alias| !is_keyword(alias)) {
            bindings.push(((*alias).to_string(), relation));
        }

        index += 2;
    }

    bindings
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
                    ColumnState::Loaded(columns.iter().map(|column| column.to_string()).collect()),
                )
            })
            .collect();
        SchemaCompletions {
            syntax: Syntax::Sql,
            catalog: Arc::new(catalog),
            columns: Rc::new(RefCell::new(held)),
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
        }
    }

    fn catalog() -> SchemaCompletions {
        detached(schemas(), COLUMNS)
    }

    /// The labels offered for a caret at `|`.
    fn labels(sql: &str) -> Vec<String> {
        let offset = sql.find('|').expect("the test must mark the caret");
        let sql = sql.replace('|', "");
        catalog()
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

    #[test]
    fn a_routine_is_offered() {
        assert!(labels("SELECT recalc|").contains(&"recalculate_totals".to_string()));
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
            },
            &[("shop", "users", &["_id", "name", "email"])],
        );
        completions.syntax = Syntax::Mongo;
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
            },
            &[],
        );
        completions.syntax = Syntax::Redis;
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
            },
            &held,
        );
        schema.syntax = engine.syntax();
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
}
