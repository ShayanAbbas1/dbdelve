//! mongosh-style statements, read by DBDelve: the Mongo counterpart of
//! `sql.rs`.
//!
//! Nothing here executes JavaScript. mongosh statements are JavaScript
//! expressions, so `tree-sitter-javascript` reads them: it finds statement
//! boundaries in a half-typed buffer, steps over comments and strings, and
//! tells a regex from anything else. What this module adds is a strict walk
//! over that tree into DBDelve's own types, refusing everything outside the
//! closed language below -- `db.<collection>.<method>(…)` and its relatives,
//! over literals. Reading a statement whole is what lets it be classified
//! before it runs, which on Mongo is Read-only mode's only boundary: there is
//! no server-side hold to fall back on.
//!
//! The literal tree is DBDelve's own (`Value`); `db/mongo.rs` turns it into
//! BSON, so no driver type reaches this module.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::parsing::Parsed;
use time::{Date, PrimitiveDateTime, Time, UtcOffset};
use tree_sitter::{Node, Parser, Point, Tree};

use crate::result_grid::{NewValue, PendingRow};
use crate::sql::{Destructive, Mode, Verdict};

pub(crate) mod browse;

/// One statement of a buffer, as DBDelve reads it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Statement {
    /// Byte range in the text it was parsed from, terminating `;` excluded.
    pub(crate) span: Range<usize>,
    pub(crate) target: Target,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Show(Show),
    /// A method of the database itself: `db.runCommand(…)`, `db.stats()`, …
    Database {
        /// `getSiblingDB`'s argument, when the statement names another database.
        database: Option<String>,
        call: Call<DbMethod>,
    },
    Collection {
        database: Option<String>,
        /// Dotted names read whole, as mongosh reads them: `db.system.views`
        /// is the collection `system.views`.
        collection: String,
        call: Call<Method>,
        /// The cursor methods chained after `find` or `aggregate`, in order.
        cursor: Vec<Call<CursorMethod>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Show {
    Databases,
    Collections,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call<M> {
    pub(crate) method: M,
    pub(crate) args: Vec<Arg>,
    /// From the `.` before the method's name through its `)`: the text a
    /// splice replaces (an existing `.sort(…)`), or inserts after (`find(…)`).
    pub(crate) span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Arg {
    pub(crate) value: Value,
    pub(crate) span: Range<usize>,
    /// Each element's span when the argument is an array literal, so a
    /// pipeline's stages can be found in the text without reprinting it.
    pub(crate) items: Vec<Range<usize>>,
}

/// A literal: JSON plus the shell's constructors.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Double(f64),
    /// Kept as written: Decimal128's own text is its value.
    Decimal128(String),
    String(String),
    /// `None` is `ObjectId()`, a fresh one minted where the statement is sent.
    ObjectId(Option<[u8; 12]>),
    /// Milliseconds since the Unix epoch, UTC.
    Date(i64),
    Binary {
        subtype: u8,
        bytes: Vec<u8>,
    },
    Timestamp {
        t: u32,
        i: u32,
    },
    Regex {
        pattern: String,
        flags: String,
    },
    Code(String),
    MinKey,
    MaxKey,
    Document(Vec<(String, Value)>),
    Array(Vec<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) message: String,
    /// Byte offset into the parsed text.
    pub(crate) at: usize,
}

fn error(at: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        message: message.into(),
        at,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Find,
    FindOne,
    Aggregate,
    CountDocuments,
    EstimatedDocumentCount,
    Distinct,
    GetIndexes,
    InsertOne,
    InsertMany,
    UpdateOne,
    UpdateMany,
    ReplaceOne,
    DeleteOne,
    DeleteMany,
    FindOneAndUpdate,
    FindOneAndReplace,
    FindOneAndDelete,
    CreateIndex,
    CreateIndexes,
    DropIndex,
    DropIndexes,
    Drop,
    RenameCollection,
}

impl Method {
    const ALL: [Method; 23] = [
        Method::Find,
        Method::FindOne,
        Method::Aggregate,
        Method::CountDocuments,
        Method::EstimatedDocumentCount,
        Method::Distinct,
        Method::GetIndexes,
        Method::InsertOne,
        Method::InsertMany,
        Method::UpdateOne,
        Method::UpdateMany,
        Method::ReplaceOne,
        Method::DeleteOne,
        Method::DeleteMany,
        Method::FindOneAndUpdate,
        Method::FindOneAndReplace,
        Method::FindOneAndDelete,
        Method::CreateIndex,
        Method::CreateIndexes,
        Method::DropIndex,
        Method::DropIndexes,
        Method::Drop,
        Method::RenameCollection,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Method::Find => "find",
            Method::FindOne => "findOne",
            Method::Aggregate => "aggregate",
            Method::CountDocuments => "countDocuments",
            Method::EstimatedDocumentCount => "estimatedDocumentCount",
            Method::Distinct => "distinct",
            Method::GetIndexes => "getIndexes",
            Method::InsertOne => "insertOne",
            Method::InsertMany => "insertMany",
            Method::UpdateOne => "updateOne",
            Method::UpdateMany => "updateMany",
            Method::ReplaceOne => "replaceOne",
            Method::DeleteOne => "deleteOne",
            Method::DeleteMany => "deleteMany",
            Method::FindOneAndUpdate => "findOneAndUpdate",
            Method::FindOneAndReplace => "findOneAndReplace",
            Method::FindOneAndDelete => "findOneAndDelete",
            Method::CreateIndex => "createIndex",
            Method::CreateIndexes => "createIndexes",
            Method::DropIndex => "dropIndex",
            Method::DropIndexes => "dropIndexes",
            Method::Drop => "drop",
            Method::RenameCollection => "renameCollection",
        }
    }

    /// The fewest and most arguments mongosh's method takes.
    fn arity(self) -> (usize, usize) {
        match self {
            Method::Find | Method::FindOne => (0, 3),
            Method::Aggregate | Method::CountDocuments => (0, 2),
            Method::EstimatedDocumentCount | Method::DropIndexes | Method::Drop => (0, 1),
            Method::GetIndexes => (0, 0),
            Method::Distinct | Method::CreateIndex | Method::CreateIndexes => (1, 3),
            Method::InsertOne
            | Method::InsertMany
            | Method::DeleteOne
            | Method::DeleteMany
            | Method::FindOneAndDelete
            | Method::RenameCollection => (1, 2),
            Method::DropIndex => (1, 1),
            Method::UpdateOne
            | Method::UpdateMany
            | Method::ReplaceOne
            | Method::FindOneAndUpdate
            | Method::FindOneAndReplace => (2, 3),
        }
    }

    fn returns_cursor(self) -> bool {
        matches!(self, Method::Find | Method::Aggregate)
    }

    /// `countDocuments` and `distinct` return no cursor, but a plan is asked
    /// for with the same `.explain(verbosity)` on the end.
    fn takes_explain(self) -> bool {
        matches!(
            self,
            Method::Find | Method::Aggregate | Method::CountDocuments | Method::Distinct
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DbMethod {
    RunCommand,
    AdminCommand,
    GetCollectionNames,
    Stats,
    CreateCollection,
    CreateView,
    DropDatabase,
}

impl DbMethod {
    const ALL: [DbMethod; 7] = [
        DbMethod::RunCommand,
        DbMethod::AdminCommand,
        DbMethod::GetCollectionNames,
        DbMethod::Stats,
        DbMethod::CreateCollection,
        DbMethod::CreateView,
        DbMethod::DropDatabase,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            DbMethod::RunCommand => "runCommand",
            DbMethod::AdminCommand => "adminCommand",
            DbMethod::GetCollectionNames => "getCollectionNames",
            DbMethod::Stats => "stats",
            DbMethod::CreateCollection => "createCollection",
            DbMethod::CreateView => "createView",
            DbMethod::DropDatabase => "dropDatabase",
        }
    }

    fn arity(self) -> (usize, usize) {
        match self {
            DbMethod::RunCommand | DbMethod::AdminCommand | DbMethod::CreateCollection => (1, 2),
            DbMethod::GetCollectionNames => (0, 0),
            DbMethod::Stats | DbMethod::DropDatabase => (0, 1),
            DbMethod::CreateView => (3, 4),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorMethod {
    Sort,
    Limit,
    Skip,
    Projection,
    Hint,
    Collation,
    Comment,
    MaxTimeMs,
    Explain,
    /// Accepted and meaningless: every result is fetched as an array already.
    ToArray,
    /// Accepted and meaningless: the grid lays results out.
    Pretty,
}

impl CursorMethod {
    const ALL: [CursorMethod; 11] = [
        CursorMethod::Sort,
        CursorMethod::Limit,
        CursorMethod::Skip,
        CursorMethod::Projection,
        CursorMethod::Hint,
        CursorMethod::Collation,
        CursorMethod::Comment,
        CursorMethod::MaxTimeMs,
        CursorMethod::Explain,
        CursorMethod::ToArray,
        CursorMethod::Pretty,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            CursorMethod::Sort => "sort",
            CursorMethod::Limit => "limit",
            CursorMethod::Skip => "skip",
            CursorMethod::Projection => "projection",
            CursorMethod::Hint => "hint",
            CursorMethod::Collation => "collation",
            CursorMethod::Comment => "comment",
            CursorMethod::MaxTimeMs => "maxTimeMS",
            CursorMethod::Explain => "explain",
            CursorMethod::ToArray => "toArray",
            CursorMethod::Pretty => "pretty",
        }
    }

    fn arity(self) -> (usize, usize) {
        match self {
            CursorMethod::Sort
            | CursorMethod::Limit
            | CursorMethod::Skip
            | CursorMethod::Projection
            | CursorMethod::Hint
            | CursorMethod::Collation
            | CursorMethod::Comment
            | CursorMethod::MaxTimeMs => (1, 1),
            CursorMethod::Explain => (0, 1),
            CursorMethod::ToArray | CursorMethod::Pretty => (0, 0),
        }
    }
}

/// Byte ranges of each statement in `text`, in order, trimmed, terminating `;`
/// excluded, and comments between statements outside every range.
///
/// Error-tolerant, as `sql::Buffer` is: a statement that does not parse still
/// has a range, and so does every statement around it.
pub(crate) fn statements(text: &str) -> Vec<Range<usize>> {
    read(text).into_iter().map(|(span, _)| span).collect()
}

/// Every statement in `text`, or the first error in any of them. Spans are
/// offsets into `text`.
pub(crate) fn parse(text: &str) -> Result<Vec<Statement>, ParseError> {
    read(text)
        .into_iter()
        .map(|(span, target)| {
            Ok(Statement {
                span,
                target: target?,
            })
        })
        .collect()
}

type Reading = (Range<usize>, Result<Target, ParseError>);

/// Every statement in `text` and what it reads as, in order.
///
/// The text is parsed once and each statement walked from the node the
/// grammar gave it. Parsing each statement again on its own costs a parse per
/// statement, which over a pasted buffer of thousands of inserts is quadratic
/// and runs on the UI thread. Only what the grammar could not read is parsed
/// again, alone.
//
// ponytail: text after an error that swallowed the rest of the buffer (an
// unclosed `{`) is parsed again from the cut, so k such errors cost k parses of
// what follows them. Incremental reparsing is the upgrade if a wall of broken
// statements is ever pasted.
fn read(text: &str) -> Vec<Reading> {
    let mut parser = Parser::new();
    let mut shows = show_candidates(text);
    let tree = loop {
        let source = masked(text, &shows);
        let tree = parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .ok()
            .and_then(|()| parser.parse(&source, None));
        let Some(tree) = tree else {
            // Never reached with a grammar that loads, and answered as
            // unreadable rather than as an empty buffer, which would classify
            // as a read.
            return trim_range(text, 0..text.len())
                .map(|span| {
                    (
                        span.clone(),
                        Err(error(span.start, "The grammar failed to load")),
                    )
                })
                .into_iter()
                .collect();
        };
        let before = shows.len();
        shows.retain(|show| !inside_statement(&tree, show.start));
        if shows.len() == before {
            break tree;
        }
    };
    let mut reader = Reader {
        parser,
        source: masked(text, &shows),
        lines: std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect(),
    };
    let mut readings: Vec<Reading> = shows
        .into_iter()
        .map(|span| {
            let what = text[span.start + "show".len()..span.end].trim();
            (span.clone(), show(what, span.start))
        })
        .collect();
    reader.read(&tree, &mut readings);
    readings.sort_by_key(|(span, _)| span.start);
    readings
}

fn masked(text: &str, shows: &[Range<usize>]) -> String {
    let mut masked = text.to_owned();
    for show in shows {
        masked.replace_range(show.clone(), &" ".repeat(show.len()));
    }
    masked
}

struct Reader {
    parser: Parser,
    /// The text with its `show` commands blanked out: the same length, so the
    /// same offsets.
    source: String,
    /// Where each line starts, for the points tree-sitter's ranges carry.
    lines: Vec<usize>,
}

impl Reader {
    fn read(&mut self, tree: &Tree, out: &mut Vec<Reading>) {
        let root = tree.root_node();
        let mut cursor = root.walk();
        let nodes: Vec<Node> = root.named_children(&mut cursor).collect();
        for node in nodes {
            if matches!(node.kind(), "comment" | "empty_statement") {
                continue;
            }
            // The grammar hangs a comment that trails a statement without a
            // `;` inside it; it is no part of what runs.
            let mut cursor = node.walk();
            let end = node
                .children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "comment" | ";"))
                .last()
                .map_or(node.end_byte(), |child| child.end_byte());
            let Some(piece) = trim_range(&self.source, node.start_byte()..end) else {
                continue;
            };
            if !node.has_error() {
                let walk = Walk {
                    source: &self.source,
                };
                out.push((piece, walk.statement(node)));
                continue;
            }
            match cut(tree, &self.source, node, piece.clone()) {
                Some((end, resume)) => {
                    if let Some(head) = trim_range(&self.source, piece.start..end) {
                        let reading = self.reread(head.clone());
                        out.push((head, reading));
                    }
                    if let Some(rest) = self.parse_range(resume..piece.end) {
                        self.read(&rest, out);
                    }
                }
                None => {
                    let reading = self.reread(piece.clone());
                    out.push((piece, reading));
                }
            }
        }
    }

    /// `range` of the source parsed on its own, with offsets still into the
    /// whole source.
    fn parse_range(&mut self, range: Range<usize>) -> Option<Tree> {
        let point = |byte: usize| {
            let row = self.lines.partition_point(|&start| start <= byte) - 1;
            Point::new(row, byte - self.lines[row])
        };
        let included = tree_sitter::Range {
            start_byte: range.start,
            end_byte: range.end,
            start_point: point(range.start),
            end_point: point(range.end),
        };
        self.parser.set_included_ranges(&[included]).ok()?;
        let tree = self.parser.parse(&self.source, None);
        self.parser.set_included_ranges(&[]).ok()?;
        tree
    }

    /// A statement the whole parse could not read, read alone, so the error
    /// is about it and not about the text around it.
    fn reread(&mut self, span: Range<usize>) -> Result<Target, ParseError> {
        let tree = self
            .parse_range(span.clone())
            .ok_or_else(|| error(span.start, "The parser gave up"))?;
        let root = tree.root_node();
        if let Some(bad) = first_error(root) {
            // An error running to the end of the text is the grammar giving up
            // on an unfinished statement, not on its first token.
            if bad.is_error() && bad.end_byte() >= span.end {
                return Err(error(span.end, "The statement ends before it is complete"));
            }
            let message = match bad.is_missing() {
                true => format!("Expected `{}` here", bad.kind()),
                false => format!("Unexpected `{}`", snippet(bad, &self.source)),
            };
            return Err(error(bad.start_byte(), message));
        }
        let walk = Walk {
            source: &self.source,
        };
        match parts(root).as_slice() {
            [statement] => walk.statement(*statement),
            [] => Err(error(span.start, "There is no statement here")),
            [_, next, ..] => Err(error(
                next.start_byte(),
                format!(
                    "`{}` follows a complete statement on the same line, with no `;` between them",
                    snippet(*next, &self.source)
                ),
            )),
        }
    }
}

fn trim_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = text.get(range.clone())?;
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    let trimmed = (range.start + leading)..(range.end - trailing);
    (!trimmed.is_empty()).then_some(trimmed)
}

fn is_name_char(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphanumeric()
}

/// Where `show <word>` may stand in `text`: the one shell command that is not
/// JavaScript, so the grammar cannot find it. It qualifies where a statement
/// can begin -- the start of the text or a line, or after a `;`, with blanks
/// and block comments between -- and when nothing but a `;` or a comment
/// follows the word on its line. Whether it is really outside every statement,
/// string and comment only a parse can say; see `read`.
fn show_candidates(text: &str) -> Vec<Range<usize>> {
    text.match_indices("show")
        .filter_map(|(start, _)| {
            if !opens_statement(&text[..start]) {
                return None;
            }
            let rest = &text[start + "show".len()..];
            let word = rest.trim_start_matches([' ', '\t']);
            if word.len() == rest.len() {
                return None;
            }
            let word_len = word.find(|c: char| !is_name_char(c)).unwrap_or(word.len());
            let after = word[word_len..].lines().next().unwrap_or_default().trim();
            let ends = after.is_empty()
                || after.starts_with(';')
                || after.starts_with("//")
                || after.starts_with("/*");
            let end = start + "show".len() + (rest.len() - word.len()) + word_len;
            (word_len > 0 && ends).then_some(start..end)
        })
        .collect()
}

/// Whether a statement may begin right after `before`.
fn opens_statement(mut before: &str) -> bool {
    loop {
        before = before.trim_end_matches([' ', '\t']);
        let Some(inner) = before.strip_suffix("*/") else {
            break;
        };
        match inner.rfind("/*") {
            Some(open) => before = &inner[..open],
            None => return false,
        }
    }
    before.is_empty() || before.ends_with(['\n', '\r', ';'])
}

/// Whether `at` falls inside something the grammar read as part of the
/// program -- a statement, a comment, or text it could not read -- rather
/// than between them, where a `show` line stands.
fn inside_statement(tree: &Tree, at: usize) -> bool {
    tree.root_node()
        .first_child_for_byte(at)
        .is_some_and(|child| child.start_byte() <= at)
}

/// Whether `at` falls inside a comment, string or regex, where nothing is a
/// statement boundary.
fn inside_literal(tree: &Tree, at: usize) -> bool {
    let mut node = tree.root_node().descendant_for_byte_range(at, at + 1);
    while let Some(current) = node {
        if matches!(
            current.kind(),
            "comment" | "string" | "template_string" | "regex"
        ) {
            return true;
        }
        node = current.parent();
    }
    false
}

/// Where an erroneous statement should be cut: the end of its first piece and
/// where the rest resumes. A statement holding an error may have swallowed the
/// ones after it -- an unclosed `{` runs to the end of the text -- so it is cut
/// at the first `;` or line opening with `db.` inside it, neither of which any
/// literal holds.
fn cut(tree: &Tree, source: &str, node: Node, piece: Range<usize>) -> Option<(usize, usize)> {
    let semicolon = leaves(node)
        .into_iter()
        .find(|leaf| leaf.kind() == ";" && leaf.start_byte() < piece.end)
        .map(|leaf| (leaf.start_byte(), leaf.end_byte()));
    let line = source[piece.clone()]
        .match_indices('\n')
        .map(|(n, _)| piece.start + n + 1)
        .map(|start| start + source[start..].len() - source[start..].trim_start().len())
        .find(|&start| {
            start < piece.end && source[start..].starts_with("db.") && !inside_literal(tree, start)
        })
        .map(|start| (start, start));
    [semicolon, line].into_iter().flatten().min()
}

fn leaves(node: Node) -> Vec<Node> {
    if node.child_count() == 0 {
        return vec![node];
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    children.into_iter().flat_map(leaves).collect()
}

fn show(what: &str, at: usize) -> Result<Target, ParseError> {
    match what {
        "dbs" => Ok(Target::Show(Show::Databases)),
        "collections" => Ok(Target::Show(Show::Collections)),
        other => Err(error(
            at,
            format!(
                "`show {other}` is not one DBDelve reads; it reads `show dbs` and `show collections`"
            ),
        )),
    }
}

/// The node's text, cut to its first line and a readable length for a message.
fn snippet(node: Node, source: &str) -> String {
    let text = &source[node.byte_range()];
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(40) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}

/// The first node the grammar could not read, or inserted because it was
/// missing.
fn first_error(node: Node) -> Option<Node> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    if !node.has_error() {
        return None;
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    children.into_iter().find_map(first_error)
}

/// Named children without the comments the grammar hangs anywhere.
fn parts(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect()
}

/// One step of a `db.…` chain, outermost last.
enum Link<'t> {
    Member {
        name: &'t str,
        dot: usize,
        at: usize,
    },
    Call(Node<'t>),
}

type Links<'t> = std::iter::Peekable<std::vec::IntoIter<Link<'t>>>;

fn next_call<'t>(links: &mut Links<'t>) -> Option<Node<'t>> {
    match links.peek() {
        Some(Link::Call(arguments)) => {
            let arguments = *arguments;
            links.next();
            Some(arguments)
        }
        _ => None,
    }
}

/// A strict reading of the tree: anything outside the language DBDelve reads
/// is an error, never skipped.
struct Walk<'s> {
    source: &'s str,
}

impl<'s> Walk<'s> {
    fn text(&self, node: Node) -> &'s str {
        &self.source[node.byte_range()]
    }

    /// An identifier as written. JavaScript reads `\u0024out` as `$out`,
    /// so a name spelled with an escape is refused rather than taken as its
    /// raw text: the name checked has to be the name the server is sent.
    fn name(&self, node: Node) -> Result<&'s str, ParseError> {
        let text = self.text(node);
        match text.contains('\\') {
            true => Err(error(
                node.start_byte(),
                format!("`{text}` spells a name with an escape, which DBDelve does not read"),
            )),
            false => Ok(text),
        }
    }

    fn unsupported(&self, node: Node, what: &str) -> ParseError {
        error(
            node.start_byte(),
            format!(
                "`{}` is not {what} DBDelve reads",
                snippet(node, self.source)
            ),
        )
    }

    fn statement(&self, node: Node<'s>) -> Result<Target, ParseError> {
        match (node.kind(), parts(node).as_slice()) {
            ("expression_statement", [expression]) => self.target(*expression),
            _ => Err(self.unsupported(node, "a statement")),
        }
    }

    fn field<'t>(&self, node: Node<'t>, name: &str) -> Result<Node<'t>, ParseError> {
        node.child_by_field_name(name)
            .ok_or_else(|| self.unsupported(node, "something"))
    }

    /// `node` flattened into the `db` it starts from and the steps after it.
    fn links(&self, node: Node<'s>, out: &mut Vec<Link<'s>>) -> Result<(), ParseError> {
        match node.kind() {
            "identifier" if self.name(node)? == "db" => Ok(()),
            "identifier" => Err(error(
                node.start_byte(),
                format!(
                    "A statement starts with `db` or `show`, not `{}`",
                    self.text(node)
                ),
            )),
            "member_expression" => {
                self.links(self.field(node, "object")?, out)?;
                let mut cursor = node.walk();
                let dot = node
                    .children(&mut cursor)
                    .find(|child| child.kind() == ".")
                    .ok_or_else(|| self.unsupported(node, "an access"))?;
                let property = self.field(node, "property")?;
                if property.kind() != "property_identifier" {
                    return Err(self.unsupported(property, "a name"));
                }
                out.push(Link::Member {
                    name: self.name(property)?,
                    dot: dot.start_byte(),
                    at: property.start_byte(),
                });
                Ok(())
            }
            "call_expression" => {
                self.links(self.field(node, "function")?, out)?;
                let arguments = self.field(node, "arguments")?;
                if arguments.kind() != "arguments" {
                    return Err(self.unsupported(arguments, "an argument list"));
                }
                out.push(Link::Call(arguments));
                Ok(())
            }
            _ => Err(self.unsupported(node, "part of a statement")),
        }
    }

    fn member(
        &self,
        links: &mut Links<'s>,
        end: usize,
        wanted: &str,
    ) -> Result<(&'s str, usize, usize), ParseError> {
        match links.next() {
            Some(Link::Member { name, dot, at }) => Ok((name, dot, at)),
            Some(Link::Call(arguments)) => Err(self.unsupported(arguments, "a call")),
            None => Err(error(
                end,
                format!("The statement ends where {wanted} should follow"),
            )),
        }
    }

    fn target(&self, expression: Node<'s>) -> Result<Target, ParseError> {
        let mut links = Vec::new();
        self.links(expression, &mut links)?;
        let end = expression.end_byte();
        let mut links = links.into_iter().peekable();

        let mut database = None;
        let collection = loop {
            let (name, dot, at) = self.member(&mut links, end, "a collection or method name")?;
            let Some(arguments) = next_call(&mut links) else {
                break name.to_owned();
            };
            match name {
                "getSiblingDB" => database = Some(self.name_argument(name, at, arguments)?),
                "getCollection" => break self.name_argument(name, at, arguments)?,
                _ => {
                    let method = DbMethod::ALL
                        .into_iter()
                        .find(|method| method.name() == name)
                        .ok_or_else(|| {
                            error(at, format!("`db.{name}` is not a method DBDelve reads"))
                        })?;
                    let call = self.call(method, name, method.arity(), dot, arguments)?;
                    if let Some(Link::Member { dot, .. }) = links.next() {
                        return Err(error(dot, format!("Nothing can be chained after `{name}`")));
                    }
                    return Ok(Target::Database { database, call });
                }
            }
        };

        let mut collection = collection;
        let (name, dot, at, arguments) = loop {
            let (name, dot, at) = self.member(&mut links, end, "`.` and a collection method")?;
            match next_call(&mut links) {
                Some(arguments) => break (name, dot, at, arguments),
                None => {
                    collection.push('.');
                    collection.push_str(name);
                }
            }
        };
        let method = Method::ALL
            .into_iter()
            .find(|method| method.name() == name)
            .ok_or_else(|| {
                error(
                    at,
                    format!("`{name}` is not a collection method DBDelve reads"),
                )
            })?;
        let call = self.call(method, name, method.arity(), dot, arguments)?;

        let mut cursor: Vec<Call<CursorMethod>> = Vec::new();
        while links.peek().is_some() {
            let (link, dot, at) = self.member(&mut links, end, "a cursor method")?;
            if !method.returns_cursor() && !(method.takes_explain() && link == "explain") {
                return Err(error(
                    dot,
                    format!("`{name}` returns no cursor, so nothing can be chained after it"),
                ));
            }
            if cursor
                .last()
                .is_some_and(|call| call.method == CursorMethod::Explain)
            {
                return Err(error(
                    dot,
                    "`explain` returns a plan, so nothing can be chained after it",
                ));
            }
            let chained = CursorMethod::ALL
                .into_iter()
                .find(|method| method.name() == link)
                .ok_or_else(|| {
                    error(at, format!("`{link}` is not a cursor method DBDelve reads"))
                })?;
            let arguments = next_call(&mut links).ok_or_else(|| {
                error(at, format!("`{link}` is a method, and is not called here"))
            })?;
            cursor.push(self.call(chained, link, chained.arity(), dot, arguments)?);
        }
        Ok(Target::Collection {
            database,
            collection,
            call,
            cursor,
        })
    }

    fn call<M>(
        &self,
        method: M,
        name: &str,
        (fewest, most): (usize, usize),
        dot: usize,
        arguments: Node,
    ) -> Result<Call<M>, ParseError> {
        let args = self.args(arguments)?;
        if !(fewest..=most).contains(&args.len()) {
            let takes = match (fewest, most) {
                (0, 0) => "no arguments".to_owned(),
                (1, 1) => "one argument".to_owned(),
                (fewest, most) if fewest == most => format!("{fewest} arguments"),
                (fewest, most) => format!("{fewest} to {most} arguments"),
            };
            return Err(error(
                arguments.start_byte(),
                format!("`{name}` takes {takes}, and was given {}", args.len()),
            ));
        }
        Ok(Call {
            method,
            args,
            span: dot..arguments.end_byte(),
        })
    }

    fn args(&self, arguments: Node) -> Result<Vec<Arg>, ParseError> {
        parts(arguments)
            .into_iter()
            .map(|node| {
                let (value, items) = match node.kind() {
                    "array" => {
                        let (values, items) = self.array(node)?;
                        (Value::Array(values), items)
                    }
                    _ => (self.value(node)?, Vec::new()),
                };
                Ok(Arg {
                    value,
                    span: node.byte_range(),
                    items,
                })
            })
            .collect()
    }

    /// `getSiblingDB`'s and `getCollection`'s one string.
    fn name_argument(&self, name: &str, at: usize, arguments: Node) -> Result<String, ParseError> {
        match self.args(arguments)?.as_slice() {
            [
                Arg {
                    value: Value::String(text),
                    ..
                },
            ] if !text.is_empty() => Ok(text.clone()),
            _ => Err(error(at, format!("`{name}` takes one name, as a string"))),
        }
    }

    fn value(&self, node: Node) -> Result<Value, ParseError> {
        let at = node.start_byte();
        match node.kind() {
            "object" => self.document(node),
            "array" => self.array(node).map(|(values, _)| Value::Array(values)),
            "string" | "template_string" => self.string(node).map(Value::String),
            "number" => self.number(node).map(number),
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "null" => Ok(Value::Null),
            "identifier" => match self.text(node) {
                "Infinity" => Ok(Value::Double(f64::INFINITY)),
                "NaN" => Ok(Value::Double(f64::NAN)),
                _ => Err(self.unsupported(node, "a literal")),
            },
            "unary_expression" => {
                let operator = self.text(self.field(node, "operator")?);
                let argument = self.field(node, "argument")?;
                let magnitude = match (argument.kind(), self.text(argument)) {
                    ("number", _) => self.number(argument)?,
                    ("identifier", "Infinity") => f64::INFINITY,
                    ("identifier", "NaN") => f64::NAN,
                    _ => return Err(self.unsupported(node, "a literal")),
                };
                match operator {
                    "-" => Ok(number(-magnitude)),
                    "+" => Ok(number(magnitude)),
                    _ => Err(self.unsupported(node, "a literal")),
                }
            }
            "regex" => {
                let pattern = self.text(self.field(node, "pattern")?).to_owned();
                let flags = node
                    .child_by_field_name("flags")
                    .map_or("", |flags| self.text(flags));
                regex_value(pattern, flags.to_owned()).map_err(|m| error(at, m))
            }
            "new_expression" => {
                let constructor = self.field(node, "constructor")?;
                if constructor.kind() != "identifier" {
                    return Err(self.unsupported(constructor, "a constructor"));
                }
                let args = match node.child_by_field_name("arguments") {
                    Some(arguments) => self.args(arguments)?,
                    None => Vec::new(),
                };
                constructor_value(self.name(constructor)?, args, at, true)
            }
            "call_expression" => {
                let function = self.field(node, "function")?;
                let arguments = self.field(node, "arguments")?;
                if function.kind() != "identifier" || arguments.kind() != "arguments" {
                    return Err(self.unsupported(node, "a literal"));
                }
                constructor_value(self.name(function)?, self.args(arguments)?, at, false)
            }
            _ => Err(self.unsupported(node, "a literal")),
        }
    }

    /// The elements, and where each one stands in the text. A hole (`[1,,2]`)
    /// is refused rather than closed up: the grammar reads it, and dropping it
    /// would shift every element after it.
    fn array(&self, node: Node) -> Result<(Vec<Value>, Vec<Range<usize>>), ParseError> {
        let mut values = Vec::new();
        let mut items = Vec::new();
        let mut after_separator = true;
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "[" | "]" | "comment" => {}
                "," if after_separator => {
                    return Err(error(child.start_byte(), "An array has an empty slot here"));
                }
                "," => after_separator = true,
                _ => {
                    values.push(self.value(child)?);
                    items.push(child.byte_range());
                    after_separator = false;
                }
            }
        }
        Ok((values, items))
    }

    /// A document shaped as an Extended JSON wrapper is the value it wraps, so
    /// a cell copied out as `{"$oid": …}` pastes back as an ObjectId, not as a
    /// document holding a string.
    fn document(&self, node: Node) -> Result<Value, ParseError> {
        let mut fields = Vec::new();
        for pair in parts(node) {
            if pair.kind() != "pair" {
                return Err(self.unsupported(pair, "a field"));
            }
            let key = self.field(pair, "key")?;
            let key = match key.kind() {
                "property_identifier" => self.name(key)?.to_owned(),
                "string" => self.string(key)?,
                _ => return Err(self.unsupported(key, "a field name")),
            };
            fields.push((key, self.value(self.field(pair, "value")?)?));
        }
        extended_json(fields).map_err(|message| error(node.start_byte(), message))
    }

    /// A string's value, read through its escapes as JavaScript reads them. A
    /// template literal counts only without substitutions, since one with them
    /// is code. Built as UTF-16 because that is what a `\uD83D\uDE00` pair
    /// spells. A template's line breaks are read as JavaScript reads them:
    /// CRLF and a lone CR are both LF.
    fn string(&self, node: Node) -> Result<String, ParseError> {
        let mut units: Vec<u16> = Vec::new();
        for part in parts(node) {
            let text = self.text(part);
            match part.kind() {
                "string_fragment" if node.kind() == "template_string" => units.extend(
                    text.replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .encode_utf16(),
                ),
                "string_fragment" => units.extend(text.encode_utf16()),
                "escape_sequence" => {
                    escape(text, &mut units).map_err(|m| error(part.start_byte(), m))?
                }
                _ => return Err(self.unsupported(part, "part of a string")),
            }
        }
        String::from_utf16(&units).map_err(|_| {
            error(
                node.start_byte(),
                "This string holds half a surrogate pair, which is no character",
            )
        })
    }

    fn number(&self, node: Node) -> Result<f64, ParseError> {
        let text = self.text(node).replace('_', "");
        let radix = match text.get(..2) {
            Some("0x" | "0X") => Some(16),
            Some("0o" | "0O") => Some(8),
            Some("0b" | "0B") => Some(2),
            _ => None,
        };
        let legacy_octal = text.len() > 1
            && text.starts_with('0')
            && text[1..].starts_with(|c: char| c.is_ascii_digit());
        let value = match radix {
            Some(radix) => u128::from_str_radix(&text[2..], radix)
                .ok()
                .map(|n| n as f64),
            None if legacy_octal => None,
            None => text.parse::<f64>().ok(),
        };
        value.ok_or_else(|| self.unsupported(node, "a number"))
    }
}

/// One escape sequence's UTF-16 units. Octal escapes other than `\0` are
/// refused: they mean different things in strict and sloppy JavaScript.
fn escape(sequence: &str, units: &mut Vec<u16>) -> Result<(), String> {
    let body = &sequence[1..];
    let mut chars = body.chars();
    let code = match chars.next() {
        Some('n') => 0x0A,
        Some('t') => 0x09,
        Some('r') => 0x0D,
        Some('b') => 0x08,
        Some('f') => 0x0C,
        Some('v') => 0x0B,
        Some('0') if body.len() == 1 => 0,
        Some('0'..='7') => return Err(format!("`{sequence}` is an octal escape")),
        Some('8' | '9') => return Err(format!("`{sequence}` is not a valid escape")),
        Some('x' | 'u') => {
            let digits = body[1..].trim_start_matches('{').trim_end_matches('}');
            let width = body[1..].starts_with('{')
                || digits.len() == if body.starts_with('x') { 2 } else { 4 };
            match u32::from_str_radix(digits, 16) {
                Ok(code) if width && !digits.is_empty() && code <= 0x10FFFF => code,
                _ => return Err(format!("`{sequence}` is not a valid escape")),
            }
        }
        Some('\r' | '\n' | '\u{2028}' | '\u{2029}') => return Ok(()),
        Some(other) => other as u32,
        None => return Err("A string ends in a lone `\\`".into()),
    };
    match char::from_u32(code) {
        Some(c) => units.extend(c.encode_utf16(&mut [0; 2]).iter()),
        // A surrogate, half of a pair the next escape may complete.
        None => units.push(code as u16),
    }
    Ok(())
}

fn constructor_value(
    name: &str,
    args: Vec<Arg>,
    at: usize,
    with_new: bool,
) -> Result<Value, ParseError> {
    let args: Vec<Value> = args.into_iter().map(|arg| arg.value).collect();
    let takes = |wants: &str| error(at, format!("`{name}` takes {wants}"));
    let value = match (name, args.as_slice()) {
        ("ObjectId", []) => Value::ObjectId(None),
        ("ObjectId", [Value::String(hex)]) => object_id(hex).map_err(|m| error(at, m))?,
        ("ObjectId", _) => return Err(takes("a 24-digit hex string")),
        ("ISODate", []) => now(),
        ("ISODate", [Value::String(text)]) => iso_date(text).map_err(|m| error(at, m))?,
        ("ISODate", _) => return Err(takes("an ISO-8601 date string")),
        ("Date", _) if !with_new => {
            return Err(error(
                at,
                "`Date(…)` without `new` is a string in mongosh, not a date",
            ));
        }
        ("Date", []) => now(),
        ("Date", [Value::String(text)]) => match iso_millis(text) {
            Some((_, true)) => {
                return Err(error(
                    at,
                    format!(
                        "`{text}` has a time and no UTC offset, which `new Date` reads in the machine's own time zone"
                    ),
                ));
            }
            _ => iso_date(text).map_err(|m| error(at, m))?,
        },
        ("Date", [millis]) if integer(millis).is_some() => {
            Value::Date(integer(millis).expect("checked"))
        }
        ("Date", _) => return Err(takes("an ISO-8601 string or milliseconds since 1970")),
        ("NumberInt" | "Int32", [value]) => match numeric(value).map(i32::try_from) {
            Some(Ok(n)) => Value::Int32(n),
            _ => return Err(takes("a whole number that fits in 32 bits")),
        },
        ("NumberLong" | "Long", [value]) => match numeric(value) {
            Some(n) => Value::Int64(n),
            None => return Err(takes("a whole number that fits in 64 bits")),
        },
        ("NumberDecimal" | "Decimal128", [value]) => {
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Int32(n) => n.to_string(),
                Value::Double(n) if n.is_nan() => "NaN".to_owned(),
                Value::Double(n) if n.is_infinite() => {
                    format!("{}Infinity", if *n < 0.0 { "-" } else { "" })
                }
                Value::Double(n) => n.to_string(),
                _ => return Err(takes("a decimal number, as a string")),
            };
            decimal(text).map_err(|m| error(at, m))?
        }
        ("Double", [Value::Int32(n)]) => Value::Double(f64::from(*n)),
        ("Double", [Value::Double(n)]) => Value::Double(*n),
        ("Double", [Value::String(text)]) => match text.trim().parse() {
            Ok(n) => Value::Double(n),
            Err(_) => return Err(takes("a number")),
        },
        ("UUID", [Value::String(text)]) => uuid(text).map_err(|m| error(at, m))?,
        ("UUID", _) => return Err(takes("a UUID string")),
        ("BinData", [subtype, Value::String(data)]) => {
            binary(subtype, STANDARD.decode(data).ok(), "base64").map_err(|m| error(at, m))?
        }
        ("HexData", [subtype, Value::String(data)]) => {
            binary(subtype, hex::decode(data).ok(), "hex").map_err(|m| error(at, m))?
        }
        ("BinData" | "HexData", _) => return Err(takes("a subtype number and a string")),
        ("Timestamp", []) => Value::Timestamp { t: 0, i: 0 },
        ("Timestamp", [t, i]) => timestamp(t, i).map_err(|m| error(at, m))?,
        ("Timestamp", [Value::Document(fields)]) => match fields.as_slice() {
            [(t_key, t), (i_key, i)] if t_key == "t" && i_key == "i" => {
                timestamp(t, i).map_err(|m| error(at, m))?
            }
            _ => return Err(takes("`t` and `i`, or `{ t: …, i: … }`")),
        },
        ("MinKey", []) => Value::MinKey,
        ("MaxKey", []) => Value::MaxKey,
        ("RegExp", [Value::String(pattern)]) => {
            regex_value(pattern.clone(), String::new()).map_err(|m| error(at, m))?
        }
        ("RegExp", [Value::String(pattern), Value::String(flags)]) => {
            regex_value(pattern.clone(), flags.clone()).map_err(|m| error(at, m))?
        }
        ("RegExp", _) => return Err(takes("a pattern string and optional flags")),
        ("Code", [Value::String(code)]) => Value::Code(code.clone()),
        ("Code", _) => return Err(takes("its JavaScript as a string")),
        ("NumberInt" | "Int32" | "NumberLong" | "Long", _) => {
            return Err(takes(&format!(
                "one whole number, and was given {}",
                given(&args)
            )));
        }
        ("NumberDecimal" | "Decimal128", _) => {
            return Err(takes(&format!(
                "one decimal number, and was given {}",
                given(&args)
            )));
        }
        ("Double", _) => {
            return Err(takes(&format!(
                "one number, and was given {}",
                given(&args)
            )));
        }
        ("Timestamp", _) => {
            return Err(takes(&format!(
                "`t` and `i`, or `{{ t: …, i: … }}`, and was given {}",
                given(&args)
            )));
        }
        ("MinKey" | "MaxKey", _) => {
            return Err(takes(&format!(
                "no arguments, and was given {}",
                given(&args)
            )));
        }
        (other, _) => {
            return Err(error(
                at,
                format!("`{other}` is not a constructor DBDelve reads"),
            ));
        }
    };
    Ok(value)
}

/// What a constructor was handed, for a message saying why it refused.
fn given(args: &[Value]) -> String {
    match args {
        [] => "none".to_owned(),
        [value] => kind(value).to_owned(),
        many => format!("{} arguments", many.len()),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Int32(_) => "an int",
        Value::Int64(_) => "a long",
        Value::Double(_) => "a double",
        Value::Decimal128(_) => "a decimal",
        Value::String(_) => "a string",
        Value::ObjectId(_) => "an ObjectId",
        Value::Date(_) => "a date",
        Value::Binary { .. } => "binary data",
        Value::Timestamp { .. } => "a timestamp",
        Value::Regex { .. } => "a regex",
        Value::Code(_) => "code",
        Value::MinKey => "MinKey",
        Value::MaxKey => "MaxKey",
        Value::Document(_) => "a document",
        Value::Array(_) => "an array",
    }
}

/// mongosh's rule, which is js-bson's: every number is a JavaScript double,
/// sent as an Int32 when it is whole and fits, and as a Double otherwise. So
/// `1.0` is an Int32, as it is in the shell. `-0` stays a Double, since an
/// Int32 cannot carry its sign.
fn number(n: f64) -> Value {
    let whole = n.fract() == 0.0 && !(n == 0.0 && n.is_sign_negative());
    match whole && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&n) {
        true => Value::Int32(n as i32),
        false => Value::Double(n),
    }
}

/// A whole number of any of the three integral-looking kinds.
fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        // The bound is exclusive: `i64::MAX as f64` rounds up to 2^63.
        Value::Double(n) if n.fract() == 0.0 && n.abs() < 2f64.powi(63) => Some(*n as i64),
        _ => None,
    }
}

/// `integer`, or a string holding one -- how the shell's number constructors
/// take their argument.
fn numeric(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => text.trim().parse().ok(),
        other => integer(other),
    }
}

fn object_id(hex_text: &str) -> Result<Value, String> {
    let mut bytes = [0u8; 12];
    match hex::decode_to_slice(hex_text, &mut bytes) {
        Ok(()) => Ok(Value::ObjectId(Some(bytes))),
        Err(_) => Err(format!("`{hex_text}` is not an ObjectId's 24 hex digits")),
    }
}

fn uuid(text: &str) -> Result<Value, String> {
    let digits: String = text.chars().filter(|&c| c != '-').collect();
    match (digits.len(), hex::decode(&digits)) {
        (32, Ok(bytes)) => Ok(Value::Binary { subtype: 4, bytes }),
        _ => Err(format!("`{text}` is not a UUID's 32 hex digits")),
    }
}

fn binary(subtype: &Value, bytes: Option<Vec<u8>>, encoding: &str) -> Result<Value, String> {
    let subtype = integer(subtype)
        .and_then(|n| u8::try_from(n).ok())
        .ok_or("A binary subtype is a number from 0 to 255")?;
    let bytes = bytes.ok_or_else(|| format!("The binary data is not valid {encoding}"))?;
    Ok(Value::Binary { subtype, bytes })
}

fn timestamp(t: &Value, i: &Value) -> Result<Value, String> {
    let part = |value: &Value| integer(value).and_then(|n| u32::try_from(n).ok());
    match (part(t), part(i)) {
        (Some(t), Some(i)) => Ok(Value::Timestamp { t, i }),
        _ => Err("A timestamp's `t` and `i` are whole numbers from 0 to 4294967295".into()),
    }
}

/// The options MongoDB's regex engine knows. JavaScript's `g`, `y` and `d`
/// mean nothing to a query, and sending one is an error rather than a silent
/// drop.
fn regex_value(pattern: String, flags: String) -> Result<Value, String> {
    match flags.chars().find(|flag| !"imsux".contains(*flag)) {
        Some(flag) => Err(format!(
            "`{flag}` is not a regex option MongoDB knows; it knows i, m, s, u and x"
        )),
        None => Ok(Value::Regex { pattern, flags }),
    }
}

fn decimal(text: String) -> Result<Value, String> {
    let body = text.strip_prefix(['-', '+']).unwrap_or(&text);
    let (mantissa, exponent) = body.split_once(['e', 'E']).unwrap_or((body, "0"));
    let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let special = matches!(body, "Infinity" | "Inf" | "NaN");
    match special || (mantissa_ok && !exponent.is_empty() && digits(exponent)) {
        true => Ok(Value::Decimal128(text)),
        false => Err(format!("`{text}` is not a decimal number")),
    }
}

fn now() -> Value {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    Value::Date(millis)
}

/// An ISO-8601 date, read as mongosh's `ISODate` reads it: a time with no
/// offset is UTC, never the machine's own zone.
fn iso_date(text: &str) -> Result<Value, String> {
    iso_millis(text)
        .map(|(millis, _)| Value::Date(millis))
        .ok_or_else(|| format!("`{text}` is not an ISO-8601 date"))
}

/// The forms mongosh's `ISODate` takes: a date, optionally a time to the
/// minute, second or fraction (`T`, `t` or a space between), and optionally an
/// offset (`Z`, `±HH`, `±HHMM` or `±HH:MM`).
const ISO_8601: &[BorrowedFormatItem] = format_description!(
    version = 2,
    "[year]-[month]-[day][optional [[first [T][t][ ]][hour]:[minute]\
     [optional [:[second][optional [.[subsecond]]]]]\
     [optional [[first [Z][z][[offset_hour sign:mandatory][optional [[optional [:]][offset_minute]]]]]]]]]"
);

/// Milliseconds since 1970, and whether the text gives a time with no offset:
/// UTC to `ISODate`, local time to JavaScript's `Date`.
fn iso_millis(text: &str) -> Option<(i64, bool)> {
    let mut parsed = Parsed::new();
    if !parsed
        .parse_items(text.as_bytes(), ISO_8601)
        .ok()?
        .is_empty()
    {
        return None;
    }
    let date = Date::try_from(parsed).ok()?;
    let time = match parsed.hour_24() {
        Some(hour) => Time::from_hms_nano(
            hour,
            parsed.minute()?,
            parsed.second().unwrap_or(0),
            parsed.subsecond().unwrap_or(0),
        )
        .ok()?,
        None => Time::MIDNIGHT,
    };
    let zoneless =
        parsed.hour_24().is_some() && parsed.offset_hour().is_none() && !text.ends_with(['Z', 'z']);
    let offset = match parsed.offset_hour() {
        Some(_) => UtcOffset::try_from(parsed).ok()?,
        None => UtcOffset::UTC,
    };
    let nanos = PrimitiveDateTime::new(date, time)
        .assume_offset(offset)
        .unix_timestamp_nanos();
    let millis = i64::try_from(nanos.div_euclid(1_000_000)).ok()?;
    Some((millis, zoneless))
}

const WRAPPERS: [&str; 13] = [
    "$oid",
    "$date",
    "$numberLong",
    "$numberInt",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$regularExpression",
    "$timestamp",
    "$minKey",
    "$maxKey",
    "$code",
];

/// A document, or the value it wraps when it is Relaxed or Canonical Extended
/// JSON. A wrapper's key beside other keys, or around the wrong kind of value,
/// is an error, as the Extended JSON spec has it: reading it as an ordinary
/// document would store a different value from the one copied.
fn extended_json(fields: Vec<(String, Value)>) -> Result<Value, String> {
    let Some(wrapper) = fields
        .iter()
        .map(|(key, _)| key.as_str())
        .find(|key| WRAPPERS.contains(key))
    else {
        return Ok(Value::Document(fields));
    };
    let malformed =
        || format!("`{wrapper}` is an Extended JSON wrapper, and this one is malformed");
    let body = |key: &str, fields: &[(String, Value)]| -> Option<Value> {
        fields
            .iter()
            .find(|(field, _)| field == key)
            .map(|(_, value)| value.clone())
    };
    let value = match fields.as_slice() {
        [(key, value)] => match (key.as_str(), value) {
            ("$oid", Value::String(hex)) => return object_id(hex),
            ("$date", Value::String(text)) => return iso_date(text),
            ("$date", millis) if integer(millis).is_some() => {
                Value::Date(integer(millis).expect("checked"))
            }
            ("$numberLong", Value::String(text)) => {
                Value::Int64(text.parse().map_err(|_| malformed())?)
            }
            ("$numberInt", Value::String(text)) => {
                Value::Int32(text.parse().map_err(|_| malformed())?)
            }
            ("$numberDouble", Value::String(text)) => Value::Double(match text.as_str() {
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                "NaN" => f64::NAN,
                text => text.parse().map_err(|_| malformed())?,
            }),
            ("$numberDecimal", Value::String(text)) => return decimal(text.clone()),
            ("$uuid", Value::String(text)) => return uuid(text),
            ("$code", Value::String(code)) => Value::Code(code.clone()),
            ("$minKey", Value::Int32(1)) => Value::MinKey,
            ("$maxKey", Value::Int32(1)) => Value::MaxKey,
            ("$binary", Value::Document(body_fields)) if body_fields.len() == 2 => {
                match (body("base64", body_fields), body("subType", body_fields)) {
                    (Some(Value::String(data)), Some(Value::String(subtype))) => {
                        return binary_wrapper(&data, &subtype).ok_or_else(malformed);
                    }
                    _ => return Err(malformed()),
                }
            }
            ("$regularExpression", Value::Document(body_fields)) if body_fields.len() == 2 => {
                match (body("pattern", body_fields), body("options", body_fields)) {
                    (Some(Value::String(pattern)), Some(Value::String(options))) => {
                        return regex_value(pattern, options);
                    }
                    _ => return Err(malformed()),
                }
            }
            ("$timestamp", Value::Document(body_fields)) if body_fields.len() == 2 => {
                match (body("t", body_fields), body("i", body_fields)) {
                    (Some(t), Some(i)) => return timestamp(&t, &i),
                    _ => return Err(malformed()),
                }
            }
            _ => return Err(malformed()),
        },
        // The legacy spelling, which mongoexport still writes.
        [
            (binary, Value::String(data)),
            (kind, Value::String(subtype)),
        ] if binary == "$binary" && kind == "$type" => {
            return binary_wrapper(data, subtype).ok_or_else(malformed);
        }
        _ => return Err(malformed()),
    };
    Ok(value)
}

fn binary_wrapper(data: &str, subtype: &str) -> Option<Value> {
    let subtype = u8::from_str_radix(subtype, 16).ok()?;
    let bytes = STANDARD.decode(data).ok()?;
    Some(Value::Binary { subtype, bytes })
}

/// The lowest mode that may run `text`, and what makes it dangerous if anything
/// does: `sql::classify`'s answer, for a Mongo buffer.
///
/// All or nothing, as there: one statement it cannot read makes the whole
/// submission one it cannot vouch for, and that verdict is returned alone.
pub(crate) fn classify(text: &str) -> Verdict {
    // Mongo has no session-level Read-only hold, so as on SQLite an unreadable
    // statement may not run even once in Read-only.
    let unreadable = Verdict {
        mode: Mode::ReadWrite,
        destructive: vec![Destructive::Unreadable],
    };
    let Ok(statements) = parse(text) else {
        return unreadable;
    };
    statements
        .iter()
        .try_fold(Verdict::READ, |verdict, statement| {
            Some(verdict.max(statement_verdict(&statement.target)?))
        })
        .unwrap_or(unreadable)
}

/// Whether `text` is a statement `.explain(verbosity)` can be put on the end
/// of, and why not when it is not. Only a lone read is: Analyze runs what it
/// explains, so a statement that writes, or that cannot be read whole, is
/// refused here for both modes rather than trusted to the server's `explain`.
pub(crate) fn explainable(text: &str) -> Result<(), String> {
    let refusal = "Explain takes one find, aggregate, countDocuments or distinct statement.";
    let statements = parse(text).map_err(|error| error.message)?;
    let [
        Statement {
            span,
            target: Target::Collection { call, cursor, .. },
        },
    ] = statements.as_slice()
    else {
        return Err(refusal.into());
    };
    // A `;` or a comment after the statement would take the suffix with it.
    if span.end != text.trim_end().len() {
        return Err(refusal.into());
    }
    match call.method {
        Method::Find | Method::Aggregate | Method::CountDocuments | Method::Distinct => {}
        Method::FindOne
        | Method::EstimatedDocumentCount
        | Method::GetIndexes
        | Method::InsertOne
        | Method::InsertMany
        | Method::UpdateOne
        | Method::UpdateMany
        | Method::ReplaceOne
        | Method::DeleteOne
        | Method::DeleteMany
        | Method::FindOneAndUpdate
        | Method::FindOneAndReplace
        | Method::FindOneAndDelete
        | Method::CreateIndex
        | Method::CreateIndexes
        | Method::DropIndex
        | Method::DropIndexes
        | Method::Drop
        | Method::RenameCollection => return Err(refusal.into()),
    }
    if cursor
        .iter()
        .any(|call| call.method == CursorMethod::Explain)
    {
        return Err("The statement already asks for a plan.".into());
    }
    if classify(text) != Verdict::READ {
        return Err("Only a statement that reads can be explained.".into());
    }
    Ok(())
}

/// `None` for a statement whose effect cannot be read off it: a command not
/// on the whitelist.
fn statement_verdict(target: &Target) -> Option<Verdict> {
    let verdict = match target {
        Target::Show(Show::Databases | Show::Collections) => Verdict::READ,
        Target::Database { call, .. } => match call.method {
            DbMethod::RunCommand | DbMethod::AdminCommand => {
                return command_verdict(&call.args.first()?.value);
            }
            DbMethod::GetCollectionNames | DbMethod::Stats => Verdict::READ,
            DbMethod::CreateCollection | DbMethod::CreateView => Verdict::WRITE,
            DbMethod::DropDatabase => Verdict::destroys(Destructive::Drop),
        },
        Target::Collection { call, .. } => collection_verdict(call),
    };
    Some(verdict)
}

fn collection_verdict(call: &Call<Method>) -> Verdict {
    let unfiltered = !narrows(call.args.first().map(|arg| &arg.value));
    match call.method {
        Method::Find
        | Method::FindOne
        | Method::CountDocuments
        | Method::EstimatedDocumentCount
        | Method::Distinct
        | Method::GetIndexes => Verdict::READ,
        Method::Aggregate => match call.args.iter().any(|arg| writes_out(&arg.value)) {
            true => Verdict::WRITE,
            false => Verdict::READ,
        },
        Method::InsertOne
        | Method::InsertMany
        | Method::UpdateOne
        | Method::DeleteOne
        | Method::FindOneAndUpdate
        | Method::FindOneAndReplace
        | Method::FindOneAndDelete
        | Method::UpdateMany
        | Method::ReplaceOne
        | Method::CreateIndex
        | Method::CreateIndexes => Verdict::WRITE,
        // The exact counterpart of SQL's DELETE without WHERE, and the only
        // one: an unqualified UPDATE is a plain write there, so `updateMany({})`
        // is one here, and `findOneAndDelete({})` removes a single document.
        Method::DeleteMany if unfiltered => Verdict::destroys(Destructive::UnfilteredDelete),
        Method::DeleteMany => Verdict::WRITE,
        Method::Drop | Method::DropIndex | Method::DropIndexes => {
            Verdict::destroys(Destructive::Drop)
        }
        // `dropTarget` drops the collection already holding the new name.
        // Anything but an absent or literal `false` second argument is read as
        // asking for it.
        Method::RenameCollection => match call.args.get(1).map(|arg| &arg.value) {
            None | Some(Value::Bool(false)) => Verdict::WRITE,
            Some(_) => Verdict::destroys(Destructive::Drop),
        },
    }
}

/// Whether a filter names any condition. A missing or non-document filter
/// does not, and neither does a `$comment`, which only labels the operation.
fn narrows(filter: Option<&Value>) -> bool {
    matches!(
        filter,
        Some(Value::Document(fields)) if fields.iter().any(|(key, _)| key != "$comment")
    )
}

/// What a `runCommand` or `adminCommand` does, by its name. A command that has
/// a helper here classifies as its helper does, so spelling it as a command is
/// never a way past the gate the helper meets. `None` for every other command:
/// off the read list, it is unreadable, as unparseable SQL is.
fn command_verdict(command: &Value) -> Option<Verdict> {
    let (name, fields): (&str, &[(String, Value)]) = match command {
        Value::String(name) => (name, &[]),
        Value::Document(fields) => (fields.first()?.0.as_str(), fields),
        _ => return None,
    };
    let field = |key: &str| {
        fields
            .iter()
            .find(|(field, _)| field == key)
            .map(|(_, value)| value)
    };
    let verdict = match name {
        "drop"
        | "dropDatabase"
        | "dropIndexes"
        | "deleteIndexes"
        | "dropUser"
        | "dropAllUsersFromDatabase"
        | "dropRole"
        | "dropAllRolesFromDatabase"
        | "shutdown" => Verdict::destroys(Destructive::Drop),
        // `limit: 0` deletes every match, so an entry with no condition and no
        // limit of one is `deleteMany({})`.
        "delete" => {
            let deletes_all = |entry: &Value| match entry {
                Value::Document(entry) => {
                    let get = |key: &str| entry.iter().find(|(k, _)| k == key).map(|(_, v)| v);
                    !narrows(get("q")) && get("limit").and_then(integer) != Some(1)
                }
                _ => false,
            };
            match field("deletes") {
                Some(Value::Array(entries)) if entries.iter().any(deletes_all) => {
                    Verdict::destroys(Destructive::UnfilteredDelete)
                }
                _ => Verdict::WRITE,
            }
        }
        "renameCollection" => match field("dropTarget") {
            None | Some(Value::Bool(false)) => Verdict::WRITE,
            Some(_) => Verdict::destroys(Destructive::Drop),
        },
        "insert" | "update" | "findAndModify" | "createIndexes" | "create" => Verdict::WRITE,
        _ => return command_reads(command).then_some(Verdict::READ),
    };
    Some(verdict)
}

/// The commands `runCommand` may run in Read-only: each reads and nothing
/// else. Anything off the list is unreadable, as unparseable SQL is.
const READ_COMMANDS: [&str; 19] = [
    "find",
    "aggregate",
    "count",
    "distinct",
    "listCollections",
    "listIndexes",
    "listDatabases",
    "dbStats",
    "collStats",
    "serverStatus",
    "buildInfo",
    "hello",
    "isMaster",
    "ping",
    "connectionStatus",
    "currentOp",
    "hostInfo",
    "getParameter",
    "explain",
];

/// Whether a command reads and nothing else. The server dispatches on the
/// first key, compared exactly. An `explain` is only as safe as the command it
/// explains, which is held to the same list.
fn command_reads(command: &Value) -> bool {
    let (name, explained) = match command {
        Value::String(name) => (name.as_str(), None),
        Value::Document(fields) => match fields.first() {
            Some((name, value)) => (name.as_str(), Some(value)),
            None => return false,
        },
        _ => return false,
    };
    if writes_out(command) || !READ_COMMANDS.contains(&name) {
        return false;
    }
    match (name, explained) {
        ("explain", Some(inner @ Value::Document(_))) => command_reads(inner),
        ("explain", _) => false,
        _ => true,
    }
}

/// Whether a `$out` or `$merge` stage appears anywhere in `value`. Searched
/// for at any depth rather than only at the top of a pipeline: the server
/// refuses one nested where it cannot run, and a classifier that looked only
/// where it can would be trusting that refusal.
fn writes_out(value: &Value) -> bool {
    match value {
        Value::Document(fields) => fields
            .iter()
            .any(|(key, value)| key == "$out" || key == "$merge" || writes_out(value)),
        Value::Array(values) => values.iter().any(writes_out),
        _ => false,
    }
}

/// Each pending row as one `updateOne` that names the document by its key
/// and `$set`s each edited field, one statement per row, in order.
///
/// Nothing brackets the batch (`Engine::transaction_start` is `None`): the
/// statements run one at a time, and a failure says which one failed and
/// that the ones before it ran.
///
/// Each new value is read as the type its cell held ([`coerce`]), and text
/// that does not read as one is the error, never a string in its place.
pub(crate) fn update_batch(rows: &[PendingRow]) -> Result<String, String> {
    if rows.is_empty() {
        return Err("There are no edits to apply.".into());
    }
    let mut statements = Vec::with_capacity(rows.len());
    for row in rows {
        let mut sets = Vec::with_capacity(row.sets.len());
        for (field, value) in &row.sets {
            if !writable_field(field) {
                return Err(unwritable_field(field));
            }
            let value = match value {
                NewValue::Value(text) => typed(field, text, type_of(&row.types, field))?,
                NewValue::Null => Value::Null,
                NewValue::Default => {
                    return Err(format!(
                        "{field}: MongoDB has no default for a field to take."
                    ));
                }
            };
            sets.push((field.clone(), value));
        }
        statements.push(format!(
            "{}.updateOne({}, {{$set: {}}});",
            collection(&row.table),
            key_filter(&row.keys, &row.types)?,
            spelled(&Value::Document(sets))
        ));
    }
    Ok(statements.join("\n"))
}

/// One `insertOne` of the fields the form filled. A field left empty is left
/// out of the document rather than written as a null; a null is the field's
/// null toggle. A field's sampled type decides how its text reads when the
/// sample saw one type there, and otherwise the text is read as a literal.
pub(crate) fn insert_row(
    table: &str,
    columns: &[(&str, Option<&str>)],
    types: &[(String, String)],
) -> Result<String, String> {
    let mut fields = Vec::with_capacity(columns.len());
    for &(field, value) in columns {
        let value = match value {
            None => Value::Null,
            Some("") => continue,
            Some(text) => typed(field, text, sampled_type(type_of(types, field)))?,
        };
        fields.push((field.to_owned(), value));
    }
    if fields.is_empty() {
        return Err("There is nothing in this row to insert.".into());
    }
    if let Some((field, _)) = fields.iter().find(|(field, _)| field.starts_with('$')) {
        return Err(unwritable_field(field));
    }
    Ok(format!(
        "{}.insertOne({})",
        collection(table),
        spelled(&Value::Document(fields))
    ))
}

/// One `deleteOne` naming the document by its key.
pub(crate) fn delete_row(
    table: &str,
    keys: &[(&str, &str)],
    types: &[(String, String)],
) -> Result<String, String> {
    let keys: Vec<(String, String)> = keys
        .iter()
        .map(|&(field, text)| (field.to_owned(), text.to_owned()))
        .collect();
    Ok(format!(
        "{}.deleteOne({})",
        collection(table),
        key_filter(&keys, types)?
    ))
}

/// Whether a `$set` can name `field` and reach that field alone: a `.` would
/// reach into a subdocument, and a leading `$` is an operator.
pub(crate) fn writable_field(field: &str) -> bool {
    !field.is_empty() && !field.contains('.') && !field.starts_with('$')
}

fn unwritable_field(field: &str) -> String {
    format!(
        "`{field}` is a field name DBDelve does not write: a `.` or a leading `$` would make it name another field or an operator."
    )
}

fn collection(table: &str) -> String {
    format!("db.getCollection({})", quoted(table))
}

/// `{_id: <value>}`, from the key's values as the server sent them.
fn key_filter(keys: &[(String, String)], types: &[(String, String)]) -> Result<String, String> {
    if keys.is_empty() {
        return Err("dbdelve cannot name an edited row by its primary key.".into());
    }
    let fields = keys
        .iter()
        .map(|(field, text)| {
            let name = match field == "_id" {
                true => field.clone(),
                false => quoted(field),
            };
            Ok(format!(
                "{name}: {}",
                spelled(&typed(field, text, type_of(types, field))?)
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(format!("{{{}}}", fields.join(", ")))
}

fn type_of<'a>(types: &'a [(String, String)], field: &str) -> Option<&'a str> {
    types
        .iter()
        .find(|(name, _)| name == field)
        .map(|(_, data_type)| data_type.as_str())
}

/// The one type a sampled field held, nulls aside (`int | null` is `int`).
fn sampled_type(data_type: Option<&str>) -> Option<&str> {
    let mut types = data_type?.split(" | ").filter(|alias| *alias != "null");
    match (types.next(), types.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

fn typed(field: &str, text: &str, tag: Option<&str>) -> Result<Value, String> {
    let value = coerce(text, tag).map_err(|message| format!("{field}: {message}"))?;
    match operator_free(&value) {
        true => Ok(value),
        false => Err(format!(
            "{field}: a field name starting with `$` inside a value is an operator, which DBDelve does not write."
        )),
    }
}

/// The value `text` stands for in a cell whose value was of type `tag`, a
/// `$type` alias: the type is kept, so `42` in a long field is a long and in
/// a string field the two characters. A scalar takes its own plain spelling
/// (`42`, an ISO date, 24 hex digits for an ObjectId) or any literal of its
/// type; a document, an array and the rest take a literal. A null or a
/// missing field held no type to keep, so its text is a literal when it reads
/// as one and a string when it does not.
pub(crate) fn coerce(text: &str, tag: Option<&str>) -> Result<Value, String> {
    let tag = tag.unwrap_or("missing");
    let trimmed = text.trim();
    let plain = match tag {
        "null" | "missing" => {
            return Ok(read_literal(text).unwrap_or_else(|| Value::String(text.to_owned())));
        }
        "string" => return Ok(Value::String(text.to_owned())),
        "int" => trimmed.parse().ok().map(Value::Int32),
        "long" => trimmed.parse().ok().map(Value::Int64),
        "double" => trimmed.parse().ok().map(Value::Double),
        "decimal" => decimal(trimmed.to_owned()).ok(),
        "bool" => trimmed.parse().ok().map(Value::Bool),
        "date" => iso_date(trimmed).ok(),
        "objectId" => object_id(trimmed).ok(),
        _ => None,
    };
    let value = plain.or_else(|| match (tag, read_literal(text)?) {
        ("long", Value::Int32(n)) => Some(Value::Int64(n.into())),
        ("double", Value::Int32(n)) => Some(Value::Double(n.into())),
        (_, value) => Some(value),
    });
    match value {
        Some(value) if alias(&value) == tag => Ok(value),
        _ => {
            let shown: String = text.chars().take(40).collect();
            let more = if shown.len() < text.len() { "…" } else { "" };
            Err(format!(
                "`{shown}{more}` does not read as a value of type {tag}."
            ))
        }
    }
}

/// One literal standing alone, read as a statement's argument is. In
/// parentheses so a document is not read as a block, and with a newline
/// before the closing one so a trailing `//` comment cannot swallow it.
fn read_literal(text: &str) -> Option<Value> {
    let source = format!("({text}\n)");
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let [statement] = parts(root)[..] else {
        return None;
    };
    let [group] = parts(statement)[..] else {
        return None;
    };
    let [value] = parts(group)[..] else {
        return None;
    };
    if statement.kind() != "expression_statement" || group.kind() != "parenthesized_expression" {
        return None;
    }
    Walk { source: &source }.value(value).ok()
}

/// The `$type` alias of a value's type.
fn alias(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int32(_) => "int",
        Value::Int64(_) => "long",
        Value::Double(_) => "double",
        Value::Decimal128(_) => "decimal",
        Value::String(_) => "string",
        Value::ObjectId(_) => "objectId",
        Value::Date(_) => "date",
        Value::Binary { .. } => "binData",
        Value::Timestamp { .. } => "timestamp",
        Value::Regex { .. } => "regex",
        Value::Code(_) => "javascript",
        Value::MinKey => "minKey",
        Value::MaxKey => "maxKey",
        Value::Document(_) => "object",
        Value::Array(_) => "array",
    }
}

/// A value as a statement spells it, in the shell's constructors where a
/// bare spelling would read back as another type: `NumberLong("5")`, never
/// `5`, which is an int.
pub(crate) fn spelled(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => format!("NumberLong(\"{n}\")"),
        Value::Double(n) if n.is_finite() && n.fract() != 0.0 => format!("{n:?}"),
        Value::Double(n) => format!(
            "Double({})",
            match n {
                n if n.is_nan() => "NaN".to_owned(),
                n if n.is_infinite() && *n > 0.0 => "Infinity".to_owned(),
                n if n.is_infinite() => "-Infinity".to_owned(),
                n => format!("{n:?}"),
            }
        ),
        Value::Decimal128(text) => format!("NumberDecimal({})", quoted(text)),
        Value::String(text) => quoted(text),
        Value::ObjectId(Some(bytes)) => format!("ObjectId(\"{}\")", hex::encode(bytes)),
        Value::ObjectId(None) => "ObjectId()".into(),
        Value::Date(millis) => {
            const ISO: &[BorrowedFormatItem] = format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
            );
            time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(*millis) * 1_000_000)
                .ok()
                .and_then(|at| at.format(ISO).ok())
                .filter(|iso| iso_millis(iso) == Some((*millis, false)))
                .map_or_else(
                    || format!("{{$date: {{$numberLong: \"{millis}\"}}}}"),
                    |iso| format!("ISODate(\"{iso}\")"),
                )
        }
        Value::Binary { subtype, bytes } => {
            format!("BinData({subtype}, \"{}\")", STANDARD.encode(bytes))
        }
        Value::Timestamp { t, i } => format!("Timestamp({{ t: {t}, i: {i} }})"),
        Value::Regex { pattern, flags } => {
            format!("RegExp({}, {})", quoted(pattern), quoted(flags))
        }
        Value::Code(code) => format!("Code({})", quoted(code)),
        Value::MinKey => "MinKey()".into(),
        Value::MaxKey => "MaxKey()".into(),
        Value::Document(fields) if fields.is_empty() => "{}".into(),
        Value::Document(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!("{}: {}", quoted(key), spelled(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(spelled).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// A JavaScript string literal, which a JSON string always is.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

/// No `$`-named field anywhere inside: with the Extended JSON wrappers already
/// read into the values they wrap, what is left starting with `$` is an
/// operator, and a literal holds none.
fn operator_free(value: &Value) -> bool {
    match value {
        Value::Document(fields) => fields
            .iter()
            .all(|(key, value)| !key.starts_with('$') && operator_free(value)),
        Value::Array(values) => values.iter().all(operator_free),
        _ => true,
    }
}

/// Whether `text` is a write DBDelve's grid could have generated, read back
/// out of the parse rather than trusted because `update_batch`, `insert_row`
/// or `delete_row` wrote it: `sql::is_generated_write` for MongoDB.
///
/// Exactly one of three shapes, on a collection of the connected database,
/// with no options and no cursor:
///
/// - one or more `updateOne`s on one collection, each filter exactly
///   `{_id: <literal>}` and each update exactly `{$set: {…}}` over distinct
///   plain field names;
/// - one `insertOne` of a non-empty document;
/// - one `deleteOne` whose filter is exactly `{_id: <literal>}`.
///
/// Every value is a literal with no `$`-named field anywhere inside it, so no
/// operator -- `$where` included -- can ride in on a value.
pub(crate) fn is_generated_write(text: &str) -> bool {
    let Ok(statements) = parse(text) else {
        return false;
    };
    let calls: Option<Vec<(&String, &Call<Method>)>> = statements
        .iter()
        .map(|statement| match &statement.target {
            Target::Collection {
                database: None,
                collection,
                call,
                cursor,
            } if cursor.is_empty() => Some((collection, call)),
            _ => None,
        })
        .collect();
    let Some(calls) = calls else {
        return false;
    };
    let Some((first, _)) = calls.first() else {
        return false;
    };
    if calls.iter().any(|(collection, _)| collection != first) {
        return false;
    }
    match calls.as_slice() {
        [(_, call)] if call.method == Method::InsertOne => {
            matches!(arguments(call)[..], [document @ Value::Document(fields)]
                if !fields.is_empty() && distinct(fields) && operator_free(document))
        }
        [(_, call)] if call.method == Method::DeleteOne => {
            matches!(arguments(call)[..], [filter] if names_by_id(filter))
        }
        calls => calls.iter().all(|(_, call)| {
            call.method == Method::UpdateOne
                && matches!(arguments(call)[..], [filter, update]
                    if names_by_id(filter) && sets_only(update))
        }),
    }
}

/// `sql::delete_matches_key` for MongoDB: whether `text` is one `deleteOne`
/// whose filter names exactly `keys`, as a set. A readout, not a gate.
pub(crate) fn delete_matches_key(text: &str, keys: &[&str]) -> bool {
    let Ok(statements) = parse(text) else {
        return false;
    };
    let [
        Statement {
            target: Target::Collection { call, .. },
            ..
        },
    ] = statements.as_slice()
    else {
        return false;
    };
    let [
        Arg {
            value: Value::Document(fields),
            ..
        },
    ] = call.args.as_slice()
    else {
        return false;
    };
    call.method == Method::DeleteOne
        && distinct(fields)
        && fields.len() == keys.len()
        && fields
            .iter()
            .all(|(field, _)| keys.contains(&field.as_str()))
}

/// `{_id: <value>}` alone, where the value is one an equality finds only
/// itself by: not a regex, which a filter matches as a pattern; not an array,
/// which matches its elements; not null, which matches a missing field; not a
/// fresh `ObjectId()`, which names nothing yet.
fn names_by_id(filter: &Value) -> bool {
    let Value::Document(fields) = filter else {
        return false;
    };
    matches!(fields.as_slice(), [(key, value)] if key == "_id"
        && operator_free(value)
        && !matches!(value, Value::Regex { .. } | Value::Array(_) | Value::Null | Value::ObjectId(None)))
}

/// `{$set: {…}}` alone, over at least one field, each named so it reaches
/// itself alone and not `_id`, and each set to a literal.
fn sets_only(update: &Value) -> bool {
    let Value::Document(fields) = update else {
        return false;
    };
    matches!(fields.as_slice(), [(set, Value::Document(sets))] if set == "$set"
    && !sets.is_empty()
    && distinct(sets)
    && sets.iter().all(|(field, value)| {
        writable_field(field) && field != "_id" && operator_free(value)
    }))
}

fn arguments(call: &Call<Method>) -> Vec<&Value> {
    call.args.iter().map(|arg| &arg.value).collect()
}

fn distinct(fields: &[(String, Value)]) -> bool {
    fields
        .iter()
        .enumerate()
        .all(|(at, (key, _))| fields[..at].iter().all(|(other, _)| other != key))
}

/// `text` laid out over lines and indented by bracket depth: the same tokens
/// and every comment, with only the whitespace between them changed.
///
/// Token-level, as `sql::format` is through sqlformat, and for the same reason:
/// regenerating a statement from what it parsed to drops the comments and
/// respells the literals. A JavaScript formatter crate would be that
/// regeneration (dprint's and Biome's both reprint a syntax tree, and bring a
/// second parser with them), so the layout here walks the tree-sitter tokens
/// this module already reads statements with.
///
/// Refused, never guessed, when a statement does not read, and when the layout
/// would read back as anything else: a line break is a statement boundary in
/// this language, so the result is parsed again and must say exactly what the
/// buffer said. Text between statements -- comments, blank lines, the `;`s --
/// is kept as written, and so is a `show` line.
//
// ponytail: a chain outside every bracket keeps the line breaks it was written
// with, however long its line runs. Breaking before each `.sort(…)` past the
// width is the upgrade if long one-line chains turn out to be common.
pub(crate) fn format(text: &str) -> Result<String, &'static str> {
    const UNREADABLE: &str = "Not formatting: a statement in the buffer does not parse.";
    let statements = parse(text).map_err(|_| UNREADABLE)?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|_| UNREADABLE)?;

    let mut formatted = String::with_capacity(text.len());
    let mut written = 0;
    for statement in &statements {
        let span = statement.span.clone();
        formatted += &text[written..span.start];
        let source = &text[span.clone()];
        match (&statement.target, parser.parse(source, None)) {
            (Target::Show(_), _) | (_, None) => formatted += source,
            (_, Some(tree)) if tree.root_node().has_error() => formatted += source,
            (_, Some(tree)) => {
                let line = text[..span.start].rfind('\n').map_or(0, |at| at + 1);
                let indent = &text[line..span.start];
                let indent = &indent[..indent.len() - indent.trim_start_matches([' ', '\t']).len()];
                formatted += &lay_out(source, &tree, indent);
            }
        }
        written = span.end;
    }
    formatted += &text[written..];

    let reread = parse(&formatted).map_err(|_| UNREADABLE)?;
    let same = reread.len() == statements.len()
        && reread
            .iter()
            .zip(&statements)
            .all(|(after, before)| unspanned(&after.target) == unspanned(&before.target));
    match same {
        true => Ok(formatted),
        false => Err("Not formatting: the new layout would read as different statements."),
    }
}

/// How wide a bracket may stand on one line before its items go one per line.
const FORMAT_WIDTH: usize = 80;
const FORMAT_INDENT: usize = 2;

struct Token<'s> {
    text: &'s str,
    /// The source between the previous token and this one.
    gap: &'s str,
}

impl Token<'_> {
    fn is(&self, text: &str) -> bool {
        self.text == text
    }

    fn opens(&self) -> bool {
        matches!(self.text, "(" | "[" | "{")
    }

    fn closes(&self) -> bool {
        matches!(self.text, ")" | "]" | "}")
    }

    fn is_line_comment(&self) -> bool {
        self.text.starts_with("//")
    }

    fn is_comment(&self) -> bool {
        self.text.starts_with("//") || self.text.starts_with("/*")
    }
}

/// The statement's tokens: the grammar's leaves, with a string, regex or
/// comment held whole however the grammar divides it inside.
fn tokens<'s>(source: &'s str, tree: &Tree) -> Vec<Token<'s>> {
    fn collect(node: Node, out: &mut Vec<Range<usize>>) {
        let whole = matches!(
            node.kind(),
            "string" | "template_string" | "regex" | "comment"
        );
        if whole || node.child_count() == 0 {
            if !node.byte_range().is_empty() {
                out.push(node.byte_range());
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect(child, out);
        }
    }
    let mut ranges = Vec::new();
    collect(tree.root_node(), &mut ranges);
    let mut end = 0;
    ranges
        .into_iter()
        .map(|range| {
            let token = Token {
                text: &source[range.clone()],
                gap: &source[end..range.start],
            };
            end = range.end;
            token
        })
        .collect()
}

/// What stands between two tokens on one line. Whitespace is only ever
/// dropped beside punctuation; anywhere else the source's choice stands, so
/// two tokens it kept apart are never run together.
fn spacing(prev: &Token, next: &Token) -> &'static str {
    let tight = matches!(next.text, "," | ":" | ";" | ")" | "]" | ".")
        || matches!(prev.text, "(" | "[" | ".");
    if tight || (prev.is("{") && next.is("}")) {
        ""
    } else if prev.is("{") || next.is("}") || matches!(prev.text, "," | ":") {
        " "
    } else if next.is("(") || next.gap.is_empty() {
        ""
    } else {
        " "
    }
}

struct Frame {
    broken: bool,
    /// The indent of the line the bracket opened on, which its closer returns to.
    outer: usize,
}

/// One statement laid out, its continuation lines under `indent`.
fn lay_out(source: &str, tree: &Tree, indent: &str) -> String {
    let tokens = tokens(source, tree);
    let mut closer = vec![0; tokens.len()];
    let mut open = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        if token.opens() {
            open.push(at);
        } else if token.closes()
            && let Some(opener) = open.pop()
        {
            closer[opener] = at;
        }
    }
    let flat = |from: usize, to: usize| {
        (from..=to)
            .map(|at| {
                let before = match at {
                    0 => "",
                    _ if at == from => "",
                    _ => spacing(&tokens[at - 1], &tokens[at]),
                };
                before.len() + tokens[at].text.chars().count()
            })
            .sum::<usize>()
    };

    let mut out = String::new();
    let mut column = indent.chars().count();
    let mut line_indent = 0;
    let mut frames: Vec<Frame> = Vec::new();
    let mut pending = false;
    for (at, token) in tokens.iter().enumerate() {
        let inner = frames
            .iter()
            .rev()
            .find(|frame| frame.broken)
            .map_or(FORMAT_INDENT, |frame| frame.outer + FORMAT_INDENT);
        let break_to = if token.closes() {
            frames
                .last()
                .filter(|frame| frame.broken)
                .map(|frame| frame.outer)
        } else {
            let on_new_line = at > 0 && token.gap.contains('\n');
            // A comment trailing an item stays on the item's line.
            let trails = token.is_comment() && !on_new_line;
            // Outside every bracket a line break is the user's, and keeping
            // one is what keeps `.sort(…)` continuing the statement above.
            let kept = on_new_line && (token.is_comment() || frames.is_empty());
            ((pending && !trails) || kept).then_some(inner)
        };
        match break_to {
            Some(level) => {
                out.push('\n');
                out += indent;
                out += &" ".repeat(level);
                column = indent.chars().count() + level;
                line_indent = level;
                pending = false;
            }
            None if at > 0 => {
                let space = spacing(&tokens[at - 1], token);
                out += space;
                column += space.len();
            }
            None => {}
        }
        let opened_at = column;
        out += token.text;
        column = match token.text.rfind('\n') {
            Some(newline) => token.text[newline + 1..].chars().count(),
            None => column + token.text.chars().count(),
        };

        if token.opens() {
            let close = closer[at];
            let hugs = close > at + 2 && tokens[at + 1].opens() && closer[at + 1] == close - 1;
            let forced = tokens[at + 1..close].iter().any(Token::is_line_comment)
                || tokens[at + 1..=close]
                    .iter()
                    .any(|token| token.gap.contains('\n') || token.text.contains('\n'));
            let broken =
                close > at + 1 && !hugs && (forced || opened_at + flat(at, close) > FORMAT_WIDTH);
            frames.push(Frame {
                broken,
                outer: line_indent,
            });
            pending |= broken;
        } else if token.closes() {
            frames.pop();
        } else if token.is(",") && frames.last().is_some_and(|frame| frame.broken) {
            pending = true;
        }
        if token.is_line_comment() {
            pending = true;
        }
    }
    out
}

/// A statement's reading with every span dropped, so two layouts of it compare
/// equal.
fn unspanned(target: &Target) -> Target {
    fn call<M: Copy>(call: &Call<M>) -> Call<M> {
        Call {
            method: call.method,
            args: call
                .args
                .iter()
                .map(|arg| Arg {
                    value: arg.value.clone(),
                    span: 0..0,
                    items: Vec::new(),
                })
                .collect(),
            span: 0..0,
        }
    }
    match target {
        Target::Show(show) => Target::Show(*show),
        Target::Database {
            database,
            call: db_call,
        } => Target::Database {
            database: database.clone(),
            call: call(db_call),
        },
        Target::Collection {
            database,
            collection,
            call: method,
            cursor,
        } => Target::Collection {
            database: database.clone(),
            collection: collection.clone(),
            call: call(method),
            cursor: cursor.iter().map(call).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Statement {
        let mut statements = parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(statements.len(), 1, "{text}");
        statements.remove(0)
    }

    fn fails(text: &str) -> ParseError {
        parse(text).expect_err(text)
    }

    /// The first argument of a one-statement collection call.
    fn value(literal: &str) -> Value {
        match one(&format!("db.c.insertOne({literal})")).target {
            Target::Collection { call, .. } => call.args[0].value.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn literal_fails(literal: &str) -> String {
        fails(&format!("db.c.insertOne({literal})")).message
    }

    fn texts<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    fn doc(fields: &[(&str, Value)]) -> Value {
        Value::Document(
            fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    const OID: &str = "65a1b2c3d4e5f60718293a4b";

    #[test]
    fn show_reads_dbs_and_collections() {
        assert_eq!(one("show dbs").target, Target::Show(Show::Databases));
        assert_eq!(
            one("  show collections ; // all of them").target,
            Target::Show(Show::Collections)
        );
        assert_eq!(
            fails("show tables").message,
            "`show tables` is not one DBDelve reads; it reads `show dbs` and `show collections`"
        );
    }

    #[test]
    fn a_collection_is_named_by_property_by_get_collection_or_in_a_sibling_database() {
        let named = |text: &str| match one(text).target {
            Target::Collection {
                database,
                collection,
                call,
                cursor,
            } => {
                assert_eq!(call.method, Method::Find);
                assert!(cursor.is_empty());
                (database, collection)
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(named("db.users.find()"), (None, "users".into()));
        assert_eq!(
            named("db.system.views.find()"),
            (None, "system.views".into())
        );
        assert_eq!(
            named("db.getCollection(\"my-coll\").find()"),
            (None, "my-coll".into())
        );
        assert_eq!(
            named("db.getCollection('a').b.find()"),
            (None, "a.b".into())
        );
        assert_eq!(
            named("db.getSiblingDB('other').users.find()"),
            (Some("other".into()), "users".into())
        );
        assert_eq!(
            named("db.getSiblingDB('o').getCollection('c x').find()"),
            (Some("o".into()), "c x".into())
        );
    }

    #[test]
    fn database_methods_parse_with_or_without_a_sibling_database() {
        let method = |text: &str| match one(text).target {
            Target::Database { database, call } => (database, call.method),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            method("db.runCommand({ping: 1})"),
            (None, DbMethod::RunCommand)
        );
        assert_eq!(
            method("db.getSiblingDB('admin').adminCommand({listDatabases: 1})"),
            (Some("admin".into()), DbMethod::AdminCommand)
        );
        assert_eq!(
            method("db.getCollectionNames()"),
            (None, DbMethod::GetCollectionNames)
        );
        assert_eq!(method("db.stats()"), (None, DbMethod::Stats));
        assert_eq!(
            method("db.createCollection('x', {capped: true, size: 1024})"),
            (None, DbMethod::CreateCollection)
        );
        assert_eq!(
            method("db.createView('v', 'src', [{$match: {}}])"),
            (None, DbMethod::CreateView)
        );
        assert_eq!(method("db.dropDatabase()"), (None, DbMethod::DropDatabase));
        assert_eq!(
            fails("db.eval('1')").message,
            "`db.eval` is not a method DBDelve reads"
        );
        assert_eq!(
            fails("db.getSiblingDB(1).c.find()").message,
            "`getSiblingDB` takes one name, as a string"
        );
        assert!(
            fails("db.getCollection('').find()")
                .message
                .contains("getCollection")
        );
        assert_eq!(
            fails("db.stats().x()").message,
            "Nothing can be chained after `stats`"
        );
    }

    #[test]
    fn every_method_parses_by_name_at_its_fewest_arguments() {
        for method in Method::ALL {
            let args = vec!["{}"; method.arity().0].join(", ");
            match one(&format!("db.c.{}({args})", method.name())).target {
                Target::Collection { call, .. } => assert_eq!(call.method, method),
                other => panic!("{other:?}"),
            }
        }
        for method in DbMethod::ALL {
            let args = vec!["'x'"; method.arity().0].join(", ");
            match one(&format!("db.{}({args})", method.name())).target {
                Target::Database { call, .. } => assert_eq!(call.method, method),
                other => panic!("{other:?}"),
            }
        }
        for method in CursorMethod::ALL {
            let args = vec!["1"; method.arity().0].join(", ");
            match one(&format!("db.c.find().{}({args})", method.name())).target {
                Target::Collection { cursor, .. } => assert_eq!(cursor[0].method, method),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn arity_is_checked_and_named() {
        assert_eq!(
            fails("db.c.insertOne()").message,
            "`insertOne` takes 1 to 2 arguments, and was given 0"
        );
        assert_eq!(
            fails("db.c.getIndexes(1)").message,
            "`getIndexes` takes no arguments, and was given 1"
        );
        assert_eq!(
            fails("db.c.find().limit()").message,
            "`limit` takes one argument, and was given 0"
        );
        assert_eq!(fails("db.c.find().limit()").at, "db.c.find().limit".len());
        assert_eq!(one("db.c.find({}, {a: 1},)").span, 0..22);
    }

    #[test]
    fn the_cursor_chain_follows_find_and_aggregate_only() {
        let text = "db.c.find({a: 1})\n  .sort({b: -1})\n  .skip(10).limit(5)\n  .explain('executionStats')";
        let Target::Collection { call, cursor, .. } = one(text).target else {
            panic!()
        };
        assert_eq!(&text[call.span.clone()], ".find({a: 1})");
        let chained: Vec<_> = cursor.iter().map(|c| &text[c.span.clone()]).collect();
        assert_eq!(
            chained,
            [
                ".sort({b: -1})",
                ".skip(10)",
                ".limit(5)",
                ".explain('executionStats')"
            ]
        );
        one("db.c.aggregate([]).toArray()");
        one("db.c.find().pretty()");
        assert_eq!(
            fails("db.c.find().forEach(printjson)").message,
            "`forEach` is not a cursor method DBDelve reads"
        );
        assert_eq!(
            fails("db.c.insertOne({}).limit(1)").message,
            "`insertOne` returns no cursor, so nothing can be chained after it"
        );
        assert_eq!(
            fails("db.c.find().explain().limit(1)").message,
            "`explain` returns a plan, so nothing can be chained after it"
        );
        assert_eq!(
            fails("db.c.find().limit").message,
            "`limit` is a method, and is not called here"
        );
        assert_eq!(
            fails("db.c.findAll()").message,
            "`findAll` is not a collection method DBDelve reads"
        );
    }

    #[test]
    fn spans_point_into_the_text_parsed() {
        let text = "show dbs\n// a comment\ndb.a.find({x: 1})\n  .sort({y: 1});";
        let statements = parse(text).unwrap();
        assert_eq!(
            texts(
                text,
                &statements
                    .iter()
                    .map(|s| s.span.clone())
                    .collect::<Vec<_>>()
            ),
            ["show dbs", "db.a.find({x: 1})\n  .sort({y: 1})"]
        );
        let Target::Collection { call, cursor, .. } = &statements[1].target else {
            panic!()
        };
        assert_eq!(&text[call.args[0].span.clone()], "{x: 1}");
        assert_eq!(&text[cursor[0].span.clone()], ".sort({y: 1})");
    }

    #[test]
    fn a_pipelines_stages_keep_their_spans() {
        let text = "db.c.aggregate([\n  {$match: {a: 1}}, // first\n  {$sort: {b: 1}},\n])";
        let Target::Collection { call, .. } = one(text).target else {
            panic!()
        };
        let pipeline = &call.args[0];
        assert_eq!(&text[pipeline.span.clone()], &text[15..text.len() - 1]);
        assert_eq!(
            texts(text, &pipeline.items),
            ["{$match: {a: 1}}", "{$sort: {b: 1}}"]
        );
    }

    #[test]
    fn statements_must_be_whole_and_say_what_went_wrong() {
        assert_eq!(
            fails("users.find()").message,
            "A statement starts with `db` or `show`, not `users`"
        );
        assert_eq!(
            fails("db.c").message,
            "The statement ends where `.` and a collection method should follow"
        );
        assert_eq!(
            fails("db.c.find(").message,
            "The statement ends before it is complete"
        );
        assert_eq!(fails("db.c.find(").at, 10);
        assert_eq!(fails("db.c.find({a: 1)").message, "Expected `}` here");
        assert_eq!(fails("db.c.find({a: 1)").at, 15);
        let after = fails("db.a.find() db.b.find()");
        assert_eq!(after.at, 12);
        assert_eq!(after.message, "Unexpected `db`");
        assert_eq!(
            fails("db.c.find(x)").message,
            "`x` is not a literal DBDelve reads"
        );
    }

    #[test]
    fn javascript_outside_the_language_is_refused() {
        for text in [
            "var x = 1",
            "x = db.c.drop()",
            "db['c'].find()",
            "db.c?.find()",
            "db.c.find(...filters)",
            "db.c.find({...filter})",
            "db.c.find({a})",
            "db.c.find({[key]: 1})",
            "db.c.find({a() {}})",
            "db.c.find({a: x => x})",
            "db.c.find({a: function () {}})",
            "db.c.find({a: 1 + 1})",
            "db.c.find({a: `t${x}`})",
            "db.c.find({a: undefined})",
            "db.c.find({a: (1)})",
            "db.c.find({a: ObjectId.createFromHexString('x')})",
            "db.c.find({a: 'x'.trim()})",
            "db.c.find(), db.d.drop()",
            "db.c.find`x`",
            "for (;;) db.c.drop()",
            "if (true) db.c.drop()",
            "{a: 1}",
            "db",
            "(db.c.drop)()",
        ] {
            fails(text);
        }
        assert_eq!(value("`plain`"), Value::String("plain".into()));
    }

    #[test]
    fn a_name_spelled_with_an_escape_is_refused() {
        let u = |hex: &str| format!("\\u{hex}");
        for text in [
            format!("db.c.aggregate([{{{}out: 'd'}}])", u("0024")),
            format!("db.c.dr{}p()", u("006f")),
            format!("db.{}.find()", u("0063")),
            format!("{}b.c.find()", u("0064")),
            format!("db.c.insertOne({{a: ObjectI{}()}})", u("0064")),
            format!("db.c.insertOne({{a: new Dat{}(0)}})", u("0065")),
        ] {
            assert!(
                fails(&text)
                    .message
                    .contains("spells a name with an escape"),
                "{text}"
            );
            assert_eq!(classify(&text), unreadable(), "{text}");
        }
        // A quoted key's escapes are a string's, and decode as JavaScript's do.
        let quoted = format!("db.c.aggregate([{{'{}out': 'd'}}])", u("0024"));
        assert_eq!(classify(&quoted), Verdict::WRITE);
    }

    #[test]
    fn literals_take_the_shells_relaxations() {
        assert_eq!(
            value("{a: 'single', \"b\": \"double\", $c: [1, 2,], d_1: {},}"),
            doc(&[
                ("a", Value::String("single".into())),
                ("b", Value::String("double".into())),
                ("$c", Value::Array(vec![Value::Int32(1), Value::Int32(2)])),
                ("d_1", doc(&[])),
            ])
        );
        assert_eq!(
            value("{ /* before */ a: 1, // after\n b: null, c: true, d: false }"),
            doc(&[
                ("a", Value::Int32(1)),
                ("b", Value::Null),
                ("c", Value::Bool(true)),
                ("d", Value::Bool(false)),
            ])
        );
        assert_eq!(
            value("{名前: 'héllo 日本 😀'}"),
            doc(&[("名前", Value::String("héllo 日本 😀".into()))])
        );
        assert_eq!(
            value("{a: 1, a: 2}"),
            doc(&[("a", Value::Int32(1)), ("a", Value::Int32(2))])
        );
        assert_eq!(literal_fails("[1,,2]"), "An array has an empty slot here");
        assert_eq!(literal_fails("[,]"), "An array has an empty slot here");
        assert!(literal_fails("{1: 'a'}").contains("field name"));
    }

    #[test]
    fn string_escapes_read_as_javascript_reads_them() {
        let string = |literal: &str| match value(literal) {
            Value::String(text) => text,
            other => panic!("{other:?}"),
        };
        assert_eq!(string(r#""a\nb\tc\rd""#), "a\nb\tc\rd");
        assert_eq!(string(r"'it\'s'"), "it's");
        assert_eq!(string(r#""say \"hi\"""#), "say \"hi\"");
        assert_eq!(string(r#""back\\slash\/""#), "back\\slash/");
        assert_eq!(string(r#""\x41\u00e9\u{1F600}""#), "Aé😀");
        assert_eq!(string(r#""\uD83D\uDE00""#), "😀");
        assert_eq!(string("\"one \\\ntwo\""), "one two");
        assert_eq!(string(r#""\b\f\v\0""#), "\u{8}\u{c}\u{b}\0");
        assert_eq!(string(r#""\q""#), "q");
        assert_eq!(string(r#""a // b /* c */""#), "a // b /* c */");
        assert_eq!(string(r#""""#), "");
        assert!(literal_fails(r#""\uD83D""#).contains("surrogate"));
        assert!(literal_fails(r#""\12""#).contains("octal"));
        assert_eq!(literal_fails(r#""\8""#), r"`\8` is not a valid escape");
        assert_eq!(
            value("`one\r\ntwo\rthree`"),
            Value::String("one\ntwo\nthree".into())
        );
        assert_eq!(value("`kept\\r`"), Value::String("kept\r".into()));
        fails("db.c.insertOne({a: 'open})");
    }

    #[test]
    fn numbers_follow_mongoshs_int32_rule() {
        assert_eq!(value("1"), Value::Int32(1));
        assert_eq!(value("2147483647"), Value::Int32(i32::MAX));
        assert_eq!(value("2147483648"), Value::Double(2147483648.0));
        assert_eq!(value("-2147483648"), Value::Int32(i32::MIN));
        assert_eq!(value("-2147483649"), Value::Double(-2147483649.0));
        assert_eq!(value("1.0"), Value::Int32(1));
        assert_eq!(value("1e3"), Value::Int32(1000));
        assert_eq!(value("1.5"), Value::Double(1.5));
        assert_eq!(value(".5"), Value::Double(0.5));
        assert_eq!(value("+7"), Value::Int32(7));
        assert_eq!(value("1_000"), Value::Int32(1000));
        assert_eq!(value("0x1F"), Value::Int32(31));
        assert_eq!(value("0b101"), Value::Int32(5));
        assert_eq!(value("0o17"), Value::Int32(15));
        assert_eq!(value("1e300"), Value::Double(1e300));
        assert_eq!(value("Infinity"), Value::Double(f64::INFINITY));
        assert_eq!(value("-Infinity"), Value::Double(f64::NEG_INFINITY));
        assert!(matches!(value("NaN"), Value::Double(n) if n.is_nan()));
        let Value::Double(zero) = value("-0") else {
            panic!()
        };
        assert!(zero == 0.0 && zero.is_sign_negative());
        assert!(literal_fails("017").contains("`017`"));
        assert!(literal_fails("5n").contains("`5n`"));
        fails("db.c.insertOne(-'5')");
        fails("db.c.insertOne(!1)");
    }

    #[test]
    fn regex_literals_read_classes_escapes_and_flags() {
        assert_eq!(
            value(r"/a\/b[/]c;d/im"),
            Value::Regex {
                pattern: r"a\/b[/]c;d".into(),
                flags: "im".into()
            }
        );
        assert_eq!(
            value("RegExp('^a', 'i')"),
            Value::Regex {
                pattern: "^a".into(),
                flags: "i".into()
            }
        );
        assert!(literal_fails("/a/g").contains("`g`"));
        assert_eq!(
            value("/x/"),
            Value::Regex {
                pattern: "x".into(),
                flags: String::new()
            }
        );
    }

    #[test]
    fn shell_constructors_build_their_types() {
        let oid = hex::decode(OID).unwrap().try_into().unwrap();
        assert_eq!(
            value(&format!("ObjectId('{}')", OID.to_uppercase())),
            Value::ObjectId(Some(oid))
        );
        assert_eq!(
            value(&format!("new ObjectId(\"{OID}\")")),
            Value::ObjectId(Some(oid))
        );
        assert_eq!(value("ObjectId()"), Value::ObjectId(None));
        assert!(literal_fails("ObjectId('xyz')").contains("24 hex digits"));

        assert_eq!(
            value("ISODate('2024-01-15T09:30:00.123Z')"),
            Value::Date(1_705_311_000_123)
        );
        assert_eq!(
            value("new Date('2024-01-15T09:30:00.123Z')"),
            Value::Date(1_705_311_000_123)
        );
        assert_eq!(value("new Date(0)"), Value::Date(0));
        assert!(matches!(value("new Date"), Value::Date(n) if n > 1_700_000_000_000));
        assert!(matches!(value("ISODate()"), Value::Date(n) if n > 1_700_000_000_000));
        assert!(literal_fails("Date()").contains("without `new`"));
        assert_eq!(
            literal_fails("new Date('2024-01-15T09:30:00')"),
            "`2024-01-15T09:30:00` has a time and no UTC offset, which `new Date` reads in the machine's own time zone"
        );
        assert!(literal_fails("new Date('2024-01-15 09:30')").contains("no UTC offset"));
        assert_eq!(
            value("new Date('2024-01-15')"),
            Value::Date(1_705_276_800_000)
        );
        assert_eq!(
            value("new Date('2024-01-15T10:30:00+01:00')"),
            Value::Date(1_705_311_000_000)
        );
        assert_eq!(
            value("ISODate('2024-01-15T09:30:00')"),
            Value::Date(1_705_311_000_000)
        );
        assert_eq!(
            value("{$date: '2024-01-15T09:30:00'}"),
            Value::Date(1_705_311_000_000)
        );
        assert!(literal_fails("ISODate('soon')").contains("ISO-8601"));

        assert_eq!(value("NumberInt(5)"), Value::Int32(5));
        assert_eq!(value("Int32('-7')"), Value::Int32(-7));
        assert!(literal_fails("NumberInt(2147483648)").contains("32 bits"));
        assert_eq!(
            value("NumberLong('9223372036854775807')"),
            Value::Int64(i64::MAX)
        );
        assert_eq!(value("Long(42)"), Value::Int64(42));
        assert!(literal_fails("NumberLong(1.5)").contains("64 bits"));
        assert_eq!(
            value("NumberDecimal('1.10')"),
            Value::Decimal128("1.10".into())
        );
        assert_eq!(
            value("Decimal128('-1.5E+3')"),
            Value::Decimal128("-1.5E+3".into())
        );
        assert!(literal_fails("NumberDecimal('1.2.3')").contains("not a decimal"));
        assert_eq!(value("Double(1)"), Value::Double(1.0));
        assert_eq!(
            value("UUID('0e5b3c4a-1f2d-4e8a-9b7c-6d5e4f3a2b1c')"),
            Value::Binary {
                subtype: 4,
                bytes: hex::decode("0e5b3c4a1f2d4e8a9b7c6d5e4f3a2b1c").unwrap()
            }
        );
        assert!(literal_fails("UUID('0e5b')").contains("32 hex digits"));
        assert_eq!(
            value("BinData(0, 'AQID')"),
            Value::Binary {
                subtype: 0,
                bytes: vec![1, 2, 3]
            }
        );
        assert_eq!(
            value("HexData(5, '0a0b')"),
            Value::Binary {
                subtype: 5,
                bytes: vec![10, 11]
            }
        );
        assert!(literal_fails("BinData(256, 'AQID')").contains("0 to 255"));
        assert!(literal_fails("BinData(0, '!!')").contains("base64"));
        assert_eq!(
            value("Timestamp(1700000000, 3)"),
            Value::Timestamp {
                t: 1_700_000_000,
                i: 3
            }
        );
        assert_eq!(
            value("Timestamp({t: 4294967295, i: 1})"),
            Value::Timestamp { t: u32::MAX, i: 1 }
        );
        assert!(literal_fails("Timestamp(-1, 0)").contains("whole numbers"));
        assert_eq!(value("MinKey()"), Value::MinKey);
        assert_eq!(value("MaxKey()"), Value::MaxKey);
        assert_eq!(
            value("Code('function () { return 1 }')"),
            Value::Code("function () { return 1 }".into())
        );
        assert_eq!(
            literal_fails("Frobnicate(1)"),
            "`Frobnicate` is not a constructor DBDelve reads"
        );
        assert_eq!(
            literal_fails("MinKey(1)"),
            "`MinKey` takes no arguments, and was given an int"
        );
        assert_eq!(
            literal_fails("Double(NumberLong(5))"),
            "`Double` takes one number, and was given a long"
        );
        assert_eq!(
            literal_fails("NumberInt(1, 2)"),
            "`NumberInt` takes one whole number, and was given 2 arguments"
        );
        assert_eq!(
            literal_fails("Timestamp('x')"),
            "`Timestamp` takes `t` and `i`, or `{ t: …, i: … }`, and was given a string"
        );
        assert_eq!(
            value("NumberDecimal(Infinity)"),
            Value::Decimal128("Infinity".into())
        );
        assert_eq!(
            value("NumberDecimal(-Infinity)"),
            Value::Decimal128("-Infinity".into())
        );
    }

    #[test]
    fn iso_dates_take_offsets_and_reject_impossible_dates() {
        let date = |text: &str| iso_millis(text).map(|(millis, _)| millis);
        assert_eq!(date("1970-01-01"), Some(0));
        assert_eq!(date("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(date("2024-01-15 09:30"), Some(1_705_311_000_000));
        assert_eq!(date("2024-01-15T10:30:00+01:00"), Some(1_705_311_000_000));
        assert_eq!(date("2024-01-15T04:00:00-0530"), Some(1_705_311_000_000));
        assert_eq!(date("2024-01-15T11:30+02"), Some(1_705_311_000_000));
        assert_eq!(date("2024-01-15t09:30:00z"), Some(1_705_311_000_000));
        assert_eq!(date("1969-12-31T23:59:59.9995Z"), Some(-1));
        assert_eq!(date("2024-01-15T09:30:00.1Z"), Some(1_705_311_000_100));
        assert_eq!(date("2024-01-15T09:30:00.123456Z"), Some(1_705_311_000_123));
        assert_eq!(date("1969-12-31T23:59:59.999Z"), Some(-1));
        assert_eq!(date("2023-02-29"), None);
        assert_eq!(date("2024-13-01"), None);
        assert_eq!(date("2024-01-15T24:00"), None);
        assert_eq!(date("2024-01-15T09:30:00+"), None);
        assert_eq!(date("yesterday"), None);
    }

    #[test]
    fn extended_json_wrappers_read_back_as_their_values() {
        let oid = value(&format!("ObjectId('{OID}')"));
        assert_eq!(value(&format!(r#"{{"$oid": "{OID}"}}"#)), oid);
        assert_eq!(
            value(r#"{"$date": "2024-01-15T09:30:00.123Z"}"#),
            Value::Date(1_705_311_000_123)
        );
        assert_eq!(
            value(r#"{"$date": {"$numberLong": "1705311000123"}}"#),
            Value::Date(1_705_311_000_123)
        );
        assert_eq!(value(r#"{"$numberLong": "-5"}"#), Value::Int64(-5));
        assert_eq!(value(r#"{"$numberInt": "5"}"#), Value::Int32(5));
        assert_eq!(value(r#"{"$numberDouble": "5"}"#), Value::Double(5.0));
        assert_eq!(
            value(r#"{"$numberDouble": "-Infinity"}"#),
            Value::Double(f64::NEG_INFINITY)
        );
        assert_eq!(
            value(r#"{"$numberDecimal": "0.1"}"#),
            Value::Decimal128("0.1".into())
        );
        assert_eq!(
            value(r#"{"$binary": {"base64": "AQID", "subType": "80"}}"#),
            Value::Binary {
                subtype: 0x80,
                bytes: vec![1, 2, 3]
            }
        );
        assert_eq!(
            value(r#"{"$binary": "AQID", "$type": "00"}"#),
            Value::Binary {
                subtype: 0,
                bytes: vec![1, 2, 3]
            }
        );
        assert_eq!(
            value(r#"{"$uuid": "0e5b3c4a-1f2d-4e8a-9b7c-6d5e4f3a2b1c"}"#),
            value("UUID('0e5b3c4a1f2d4e8a9b7c6d5e4f3a2b1c')")
        );
        assert_eq!(
            value(r#"{"$regularExpression": {"pattern": "^a", "options": "i"}}"#),
            value("/^a/i")
        );
        assert_eq!(
            value(r#"{"$timestamp": {"t": 1, "i": 2}}"#),
            Value::Timestamp { t: 1, i: 2 }
        );
        assert_eq!(value(r#"{"$minKey": 1}"#), Value::MinKey);
        assert_eq!(value(r#"{"$maxKey": 1}"#), Value::MaxKey);
        assert_eq!(value(r#"{"$code": "x"}"#), Value::Code("x".into()));
        assert_eq!(
            value(&format!("{{_id: {{$oid: '{OID}'}}}}")),
            doc(&[("_id", oid)])
        );

        assert!(literal_fails(r#"{"$oid": "nope"}"#).contains("24 hex digits"));
        for malformed in [
            format!(r#"{{"$oid": "{OID}", "x": 1}}"#),
            r#"{"$numberInt": "2147483648"}"#.into(),
            r#"{"$minKey": 2}"#.into(),
            r#"{"$code": "x", "$scope": {}}"#.into(),
            r#"{"$binary": {"base64": "AQID"}}"#.into(),
            r#"{"x": 1, "$date": 0}"#.into(),
        ] {
            assert!(
                literal_fails(&malformed).contains("malformed"),
                "{malformed}"
            );
        }

        // Query operators are not wrappers, whatever they resemble.
        assert_eq!(
            value("{$regex: '^a', $options: 'i'}"),
            doc(&[
                ("$regex", Value::String("^a".into())),
                ("$options", Value::String("i".into())),
            ])
        );
        assert_eq!(
            value("{$type: 'string'}"),
            doc(&[("$type", Value::String("string".into()))])
        );
    }

    #[test]
    fn statements_end_at_semicolons_and_at_line_breaks_after_complete_expressions() {
        let text = "db.a.find()\ndb.b.find({x: 1});db.c.find()\n\nshow dbs; db.d.find()";
        assert_eq!(
            texts(text, &statements(text)),
            [
                "db.a.find()",
                "db.b.find({x: 1})",
                "db.c.find()",
                "show dbs",
                "db.d.find()"
            ]
        );
    }

    #[test]
    fn a_chain_continues_across_lines_and_comments() {
        let text = "db.a.find(\n  {x: 1}\n)\n  // sorted\n  .sort({x: 1})\n  .limit(5)\ndb\n  .b\n  .find()";
        assert_eq!(
            texts(text, &statements(text)),
            [
                "db.a.find(\n  {x: 1}\n)\n  // sorted\n  .sort({x: 1})\n  .limit(5)",
                "db\n  .b\n  .find()"
            ]
        );
        assert_eq!(statements("db.a.\nfind()").len(), 1);
        // JavaScript inserts no `;` before a `(`, so this is one call.
        assert_eq!(statements("db.a.drop\n()").len(), 1);
    }

    #[test]
    fn semicolons_in_strings_regexes_and_comments_end_nothing() {
        let text =
            "db.a.find({s: \";\", r: /;/}) // ; here\n/* ; and\n here */ db.b.find({s: ';'}); ";
        assert_eq!(
            texts(text, &statements(text)),
            ["db.a.find({s: \";\", r: /;/})", "db.b.find({s: ';'})"]
        );
        assert_eq!(
            statements("db.a.find() /* same line */ db.b.find()").len(),
            1
        );
    }

    #[test]
    fn show_stands_after_a_semicolon_or_a_comment_too() {
        let text = "db.a.find(); show dbs\nshow dbs; show collections\n/* c */ show dbs // all";
        assert_eq!(
            texts(text, &statements(text)),
            [
                "db.a.find()",
                "show dbs",
                "show dbs",
                "show collections",
                "show dbs"
            ]
        );
        assert_eq!(parse(text).unwrap().len(), 5);
        assert!(parse("db.a.find() /* c */ show dbs").is_err());
        assert!(parse("reshow dbs").is_err());
    }

    #[test]
    fn a_show_line_inside_a_statement_is_part_of_it() {
        let text = "db.a.find({\nshow dbs\n})";
        assert_eq!(texts(text, &statements(text)), [text]);
        assert_eq!(classify(text), unreadable());
    }

    #[test]
    fn a_buffer_of_thousands_of_statements_is_read_in_one_pass() {
        let text: String = (0..10_000)
            .map(|i| format!("db.c.insertOne({{_id: {i}, name: 'row {i}'}})\n"))
            .collect();
        let started = std::time::Instant::now();
        assert_eq!(classify(&text), Verdict::WRITE);
        assert_eq!(statements(&text).len(), 10_000);
        // A parse per statement took over 40 seconds here in a debug build; one
        // parse takes a fraction of one.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_show_inside_a_comment_or_string_is_no_statement() {
        let text = "/*\nshow dbs\n*/\ndb.a.find({s: `\nshow dbs\n`})";
        assert_eq!(
            texts(text, &statements(text)),
            ["db.a.find({s: `\nshow dbs\n`})"]
        );
        assert_eq!(statements("db.a.find({\n  show: 1\n})").len(), 1);
    }

    #[test]
    fn a_broken_statement_keeps_the_ones_around_it() {
        let text = "db.a.find()\ndb.b.find({x: \ndb.c.find()\n";
        let ranges = statements(text);
        assert_eq!(
            texts(text, &ranges),
            ["db.a.find()", "db.b.find({x:", "db.c.find()"]
        );
        let parsed: Vec<_> = ranges.iter().map(|r| parse(&text[r.clone()])).collect();
        assert!(parsed[0].is_ok() && parsed[1].is_err() && parsed[2].is_ok());

        let unclosed = "db.a.find({s: 'oops\nshow collections";
        assert_eq!(
            texts(unclosed, &statements(unclosed)),
            ["db.a.find({s: 'oops", "show collections"]
        );
        let stray = "db.a.find(}) ;db.b.find()";
        assert_eq!(
            texts(stray, &statements(stray)),
            ["db.a.find(})", "db.b.find()"]
        );
        let swallowed = "db.a.find({x: ; db.b.drop()";
        assert_eq!(
            texts(swallowed, &statements(swallowed)),
            ["db.a.find({x:", "db.b.drop()"]
        );
    }

    #[test]
    fn empty_and_comment_only_buffers_hold_no_statements() {
        assert!(statements("").is_empty());
        assert!(statements(" ;; \n// nothing\n/* here */").is_empty());
        assert_eq!(parse("// nothing").unwrap(), vec![]);
        fails("/* never closed");
    }

    fn unreadable() -> Verdict {
        Verdict {
            mode: Mode::ReadWrite,
            destructive: vec![Destructive::Unreadable],
        }
    }

    fn destroys(kind: Destructive) -> Verdict {
        Verdict::destroys(kind)
    }

    #[test]
    fn explain_is_for_a_lone_read_with_nothing_after_it() {
        for text in [
            "db.c.find({a: 1}).sort({b: 1})",
            "db.c.aggregate([{$match: {}}])  ",
            "db.c.countDocuments({a: 1})",
            "db.c.distinct('a')\n",
        ] {
            assert_eq!(explainable(text), Ok(()), "{text}");
        }
        for text in [
            "",
            "db.c.insertOne({})",
            "db.c.updateMany({}, {$set: {a: 1}})",
            "db.c.deleteMany({})",
            "db.c.drop()",
            "db.c.findOne()",
            "db.c.aggregate([{$out: 'd'}])",
            "db.c.aggregate([{$merge: 'd'}])",
            "db.runCommand({find: 'c'})",
            "show dbs",
            "db.c.find().explain()",
            "db.c.find();",
            "db.c.find() // note",
            "db.c.find(); db.c.find()",
            "db.c.find({",
        ] {
            assert!(explainable(text).is_err(), "{text}");
        }
    }

    #[test]
    fn count_and_distinct_take_only_explain_after_them() {
        one("db.c.countDocuments({a: 1}).explain('queryPlanner')");
        one("db.c.distinct('a').explain('executionStats')");
        assert_eq!(
            fails("db.c.distinct('a').limit(1)").message,
            "`distinct` returns no cursor, so nothing can be chained after it"
        );
    }

    #[test]
    fn reads_classify_as_reads() {
        for text in [
            "show dbs",
            "show collections",
            "db.getCollectionNames()",
            "db.stats()",
            "db.c.find({a: 1}).sort({b: 1}).limit(5).explain('executionStats')",
            "db.c.findOne()",
            "db.c.aggregate([{$match: {}}, {$group: {_id: '$a'}}, {$sort: {_id: 1}}])",
            "db.c.aggregate([{$lookup: {from: 'd', pipeline: [{$match: {}}], as: 'x'}}])",
            "db.c.countDocuments({})",
            "db.c.estimatedDocumentCount()",
            "db.c.distinct('a')",
            "db.c.getIndexes()",
            "db.c.aggregate([], {}, )",
            "db.getSiblingDB('other').c.find()",
            "db.c.find({$out: {$exists: true}}).limit(1)",
            "",
            "// nothing but a comment",
        ] {
            assert_eq!(classify(text), Verdict::READ, "{text}");
        }
    }

    #[test]
    fn writes_classify_as_writes() {
        for text in [
            "db.c.insertOne({a: 1})",
            "db.c.insertMany([{a: 1}, {a: 2}])",
            "db.c.updateOne({}, {$set: {a: 1}})",
            "db.c.updateMany({a: 1}, {$set: {b: 2}})",
            "db.c.replaceOne({_id: 1}, {a: 1})",
            "db.c.deleteOne({})",
            "db.c.deleteOne({_id: 1})",
            "db.c.deleteMany({a: 1})",
            "db.c.findOneAndUpdate({}, {$set: {a: 1}})",
            "db.c.findOneAndReplace({}, {a: 1})",
            "db.c.findOneAndDelete({a: 1})",
            "db.c.findOneAndDelete({})",
            "db.c.updateMany({}, {$set: {a: 1}})",
            "db.c.replaceOne({}, {a: 1})",
            "db.c.createIndex({a: 1})",
            "db.c.createIndexes([{a: 1}])",
            "db.c.renameCollection('d')",
            "db.c.renameCollection('d', false)",
            "db.createCollection('c')",
            "db.createView('v', 'c', [])",
            "db.c.aggregate([{$match: {}}, {$out: 'd'}])",
            "db.c.aggregate([{$merge: {into: 'd'}}])",
            "db.c.aggregate([{'$merge': 'd'}]).explain()",
            "db.c.aggregate([{\"\\u0024out\": 'd'}])",
            "db.c.aggregate([{$facet: {x: [{$out: 'd'}]}}])",
        ] {
            assert_eq!(classify(text), Verdict::WRITE, "{text}");
        }
    }

    #[test]
    fn drops_classify_as_destructive() {
        for text in [
            "db.c.drop()",
            "db.dropDatabase()",
            "db.getSiblingDB('prod').dropDatabase()",
            "db.c.dropIndex('a_1')",
            "db.c.dropIndexes()",
            "db.c.renameCollection('d', true)",
            "db.c.renameCollection('d', 1)",
            "db.c.renameCollection('d', {})",
        ] {
            assert_eq!(classify(text), destroys(Destructive::Drop), "{text}");
        }
    }

    #[test]
    fn a_delete_many_without_a_filter_is_destructive() {
        for text in [
            "db.c.deleteMany({})",
            "db.c.deleteMany({ /* everything */ })",
            "db.c.deleteMany({}, {w: 1})",
            "db.c.deleteMany(null)",
            "db.c.deleteMany([])",
            "db.c.deleteMany('')",
        ] {
            assert_eq!(
                classify(text),
                destroys(Destructive::UnfilteredDelete),
                "{text}"
            );
        }
    }

    #[test]
    fn run_command_reads_only_what_the_whitelist_names() {
        for command in READ_COMMANDS {
            let inner = match command {
                "explain" => "{find: 'c'}",
                _ => "1",
            };
            assert_eq!(
                classify(&format!("db.runCommand({{{command}: {inner}}})")),
                Verdict::READ,
                "{command}"
            );
        }
        assert_eq!(classify("db.adminCommand('listDatabases')"), Verdict::READ);
        assert_eq!(
            classify("db.adminCommand({ping: 1}, {comment: 'x'})"),
            Verdict::READ
        );
        assert_eq!(
            classify("db.runCommand({explain: {count: 'c'}, verbosity: 'queryPlanner'})"),
            Verdict::READ
        );
        for text in [
            "db.runCommand({killOp: 1, op: 5})",
            "db.runCommand({eval: 'db.c.drop()'})",
            "db.adminCommand({setParameter: 1, x: 1})",
            "db.runCommand({Ping: 1})",
            "db.runCommand({})",
            "db.runCommand(null)",
            "db.runCommand([{ping: 1}])",
            "db.runCommand({ping: 1, ping2: {$out: 'x'}})",
            "db.runCommand({aggregate: 'c', pipeline: [{$out: 'd'}], cursor: {}})",
            "db.runCommand({aggregate: 'c', pipeline: [{$merge: {into: 'd'}}]})",
            "db.runCommand({explain: {delete: 'c', deletes: []}})",
            "db.runCommand({explain: 'drop'})",
            "db.runCommand({explain: {explain: {drop: 'c'}}})",
            "db.runCommand({explain: {aggregate: 'c', pipeline: [{$out: 'd'}]}})",
        ] {
            assert_eq!(classify(text), unreadable(), "{text}");
        }
    }

    #[test]
    fn a_command_with_a_helper_classifies_as_its_helper_does() {
        for command in [
            "{drop: 'c'}",
            "{dropDatabase: 1}",
            "{dropIndexes: 'c', index: '*'}",
            "{deleteIndexes: 'c', index: 'a_1'}",
            "{dropUser: 'u'}",
            "{dropAllUsersFromDatabase: 1}",
            "{dropRole: 'r'}",
            "{dropAllRolesFromDatabase: 1}",
            "'shutdown'",
            "{renameCollection: 'd.a', to: 'd.b', dropTarget: true}",
            "{find: 'c'}); db.runCommand({drop: 'c'}",
        ] {
            assert_eq!(
                classify(&format!("db.adminCommand({command})")),
                destroys(Destructive::Drop),
                "{command}"
            );
        }
        for command in [
            "{delete: 'c', deletes: [{q: {}, limit: 0}]}",
            "{delete: 'c', deletes: [{q: {a: 1}, limit: 1}, {q: {$comment: 'x'}, limit: 0}]}",
            "{delete: 'c', deletes: [{limit: 0}]}",
        ] {
            assert_eq!(
                classify(&format!("db.runCommand({command})")),
                destroys(Destructive::UnfilteredDelete),
                "{command}"
            );
        }
        for command in [
            "{delete: 'c', deletes: [{q: {a: 1}, limit: 0}]}",
            "{delete: 'c', deletes: [{q: {}, limit: 1}]}",
            "{insert: 'c', documents: [{}]}",
            "{update: 'c', updates: [{q: {}, u: {$set: {a: 1}}, multi: true}]}",
            "{findAndModify: 'c', query: {}, remove: true}",
            "{createIndexes: 'c', indexes: []}",
            "{create: 'c'}",
            "{renameCollection: 'd.a', to: 'd.b'}",
            "{renameCollection: 'd.a', to: 'd.b', dropTarget: false}",
        ] {
            assert_eq!(
                classify(&format!("db.runCommand({command})")),
                Verdict::WRITE,
                "{command}"
            );
        }
    }

    #[test]
    fn a_comment_is_no_filter() {
        assert_eq!(
            classify("db.c.deleteMany({$comment: 'cleanup'})"),
            destroys(Destructive::UnfilteredDelete)
        );
        assert_eq!(
            classify("db.c.deleteMany({$comment: 'cleanup', a: 1})"),
            Verdict::WRITE
        );
    }

    #[test]
    fn anything_unparseable_is_unreadable() {
        for text in [
            "db.c.find(",
            "db.c.find() db.c.drop()",
            "db.c.remove({})",
            "db.c.find().forEach(d => db.c.deleteOne(d))",
            "db.c.find({}).deleteMany({})",
            "db['c'].drop()",
            "db.c.drоp()",
            "db.c.find({a: x})",
            "db.getCollection(name).drop()",
            "show tables",
            "use admin",
            "var c = db.c; c.drop()",
            "db.c.find(); garbage",
            "db.c.find(); db.c.drop(",
            "/* unclosed",
            "db.c.insertOne({$oid: 'nope'})",
        ] {
            assert_eq!(classify(text), unreadable(), "{text}");
        }
    }

    #[test]
    fn a_submission_is_as_dangerous_as_its_worst_statement() {
        assert_eq!(
            classify("db.a.find(); db.b.deleteMany({})"),
            destroys(Destructive::UnfilteredDelete)
        );
        assert_eq!(classify("db.a.insertOne({})\ndb.b.find()"), Verdict::WRITE);
        assert_eq!(
            classify("db.a.drop()\ndb.b.deleteMany({})"),
            Verdict {
                mode: Mode::Full,
                destructive: vec![Destructive::Drop, Destructive::UnfilteredDelete],
            }
        );
    }

    fn edited(sets: &[(&str, NewValue)], id: &str, types: &[(&str, &str)]) -> PendingRow {
        PendingRow {
            schema: "dbdelve_dev".into(),
            table: "accounts".into(),
            sets: sets
                .iter()
                .map(|(field, value)| (field.to_string(), value.clone()))
                .collect(),
            keys: vec![("_id".into(), id.into())],
            types: types
                .iter()
                .map(|(field, alias)| (field.to_string(), alias.to_string()))
                .collect(),
        }
    }

    fn typed_text(text: &str) -> NewValue {
        NewValue::Value(text.to_owned().into())
    }

    #[test]
    fn an_edit_becomes_an_update_by_id_that_sets_each_field_in_its_own_type() {
        let oid = "65a4f1c0ffffffffffffffff";
        let rows = [
            edited(
                &[
                    ("name", typed_text("Ada")),
                    ("seats", typed_text("12")),
                    ("note", NewValue::Null),
                ],
                &format!("ObjectId('{oid}')"),
                &[("_id", "objectId"), ("name", "string"), ("seats", "long")],
            ),
            edited(
                &[("tags", typed_text("['a', NumberLong(2)]"))],
                "7",
                &[("_id", "int"), ("tags", "array")],
            ),
        ];
        let batch = update_batch(&rows).unwrap();
        assert_eq!(
            batch,
            format!(
                "db.getCollection(\"accounts\").updateOne({{_id: ObjectId(\"{oid}\")}}, \
                 {{$set: {{\"name\": \"Ada\", \"seats\": NumberLong(\"12\"), \"note\": null}}}});\n\
                 db.getCollection(\"accounts\").updateOne({{_id: 7}}, \
                 {{$set: {{\"tags\": [\"a\", NumberLong(\"2\")]}}}});"
            )
        );
        assert!(is_generated_write(&batch), "{batch}");
        let parsed = parse(&batch).unwrap();
        let Target::Collection { call, .. } = &parsed[0].target else {
            panic!()
        };
        assert_eq!(
            call.args[1].value,
            Value::Document(vec![(
                "$set".into(),
                Value::Document(vec![
                    ("name".into(), Value::String("Ada".into())),
                    ("seats".into(), Value::Int64(12)),
                    ("note".into(), Value::Null),
                ])
            )])
        );
        assert!(update_batch(&[]).is_err());
    }

    #[test]
    fn text_that_does_not_read_as_its_cells_type_is_an_error_and_not_a_string() {
        for (field, alias, text) in [
            ("seats", "int", "twelve"),
            ("seats", "int", "3000000000"),
            ("seats", "long", "1.5"),
            ("joined", "date", "yesterday"),
            ("flag", "bool", "yes"),
            ("doc", "object", "[1]"),
            ("doc", "object", "{a: "),
        ] {
            let row = edited(
                &[(field, typed_text(text))],
                "1",
                &[("_id", "int"), (field, alias)],
            );
            let error = update_batch(&[row]).expect_err(text);
            assert!(error.starts_with(&format!("{field}: ")), "{error}");
        }
        let operator = edited(
            &[("doc", typed_text("{$where: 'sleep(1)'}"))],
            "1",
            &[("_id", "int"), ("doc", "object")],
        );
        assert!(update_batch(&[operator]).is_err());
        for field in ["a.b", "$x", ""] {
            let row = edited(&[(field, typed_text("1"))], "1", &[("_id", "int")]);
            assert!(update_batch(&[row]).is_err(), "{field}");
        }
        let default = edited(&[("a", NewValue::Default)], "1", &[("_id", "int")]);
        assert!(update_batch(&[default]).is_err());
    }

    #[test]
    fn each_type_reads_its_own_spellings_and_keeps_its_type() {
        let oid = [
            0x65, 0xa4, 0xf1, 0xc0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        for (text, alias, value) in [
            ("42", "string", Value::String("42".into())),
            (" 42 ", "int", Value::Int32(42)),
            ("NumberInt(42)", "int", Value::Int32(42)),
            ("42", "long", Value::Int64(42)),
            ("9223372036854775807", "long", Value::Int64(i64::MAX)),
            ("2", "double", Value::Double(2.0)),
            ("Infinity", "double", Value::Double(f64::INFINITY)),
            ("42.50", "decimal", Value::Decimal128("42.50".into())),
            ("false", "bool", Value::Bool(false)),
            (
                "2024-01-15T09:30:00.123Z",
                "date",
                Value::Date(1_705_311_000_123),
            ),
            (
                "ISODate('2024-01-15')",
                "date",
                Value::Date(1_705_276_800_000),
            ),
            (
                "65a4f1c0ffffffffffffffff",
                "objectId",
                Value::ObjectId(Some(oid)),
            ),
            (
                "ObjectId('65a4f1c0ffffffffffffffff')",
                "objectId",
                Value::ObjectId(Some(oid)),
            ),
            (
                r#"{"n": {"$numberLong": "5"}, "d": {"$numberDouble": "2.0"}}"#,
                "object",
                Value::Document(vec![
                    ("n".into(), Value::Int64(5)),
                    ("d".into(), Value::Double(2.0)),
                ]),
            ),
            (
                "[1, 'x']",
                "array",
                Value::Array(vec![Value::Int32(1), Value::String("x".into())]),
            ),
            (
                "/a/i",
                "regex",
                Value::Regex {
                    pattern: "a".into(),
                    flags: "i".into(),
                },
            ),
            (
                "Timestamp(1, 2)",
                "timestamp",
                Value::Timestamp { t: 1, i: 2 },
            ),
            ("MinKey()", "minKey", Value::MinKey),
            // A null or missing field had no type: a literal if it reads as one.
            ("42", "null", Value::Int32(42)),
            (
                "{a: 1}",
                "missing",
                Value::Document(vec![("a".into(), Value::Int32(1))]),
            ),
            (
                "plain words",
                "missing",
                Value::String("plain words".into()),
            ),
            ("", "null", Value::String(String::new())),
        ] {
            assert_eq!(coerce(text, Some(alias)), Ok(value), "{text} as {alias}");
        }
        assert!(coerce("1.5", Some("int")).is_err());
        assert!(coerce("ObjectId('nope')", Some("objectId")).is_err());
        assert!(coerce("'x'", Some("regex")).is_err());
        assert!(coerce("x", Some("symbol")).is_err());
        // A literal stands alone: nothing can ride in after it.
        assert_eq!(
            coerce("1), db.c.drop(", None),
            Ok(Value::String("1), db.c.drop(".into()))
        );
        assert_eq!(coerce("1 // note", Some("int")), Ok(Value::Int32(1)));
    }

    #[test]
    fn every_value_is_spelled_so_it_reads_back_as_itself() {
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::Int32(-5),
            Value::Int64(5),
            Value::Int64(i64::MIN),
            Value::Double(2.0),
            Value::Double(-0.0),
            Value::Double(0.1),
            Value::Double(1e300),
            Value::Double(f64::NEG_INFINITY),
            Value::Decimal128("1.50".into()),
            Value::String("quote \" and \\ and \u{2028}".into()),
            Value::ObjectId(Some([1; 12])),
            Value::Date(1_705_311_000_123),
            Value::Date(-5_000_000_000_000),
            Value::Date(i64::MAX / 2),
            Value::Binary {
                subtype: 4,
                bytes: vec![0, 255],
            },
            Value::Timestamp { t: 1, i: 2 },
            Value::Regex {
                pattern: "a/b\"".into(),
                flags: "im".into(),
            },
            Value::Code("return 'x';".into()),
            Value::MinKey,
            Value::MaxKey,
            Value::Document(Vec::new()),
            Value::Document(vec![(
                "we\"ird.key".into(),
                Value::Array(vec![Value::Int64(1)]),
            )]),
        ] {
            let spelling = spelled(&value);
            assert_eq!(read_literal(&spelling), Some(value), "{spelling}");
        }
        let nan = read_literal(&spelled(&Value::Double(f64::NAN)));
        assert!(matches!(nan, Some(Value::Double(n)) if n.is_nan()));
    }

    #[test]
    fn an_insert_leaves_out_what_the_form_left_empty() {
        let statement = insert_row(
            "accounts",
            &[
                ("_id", Some("")),
                ("name", Some("Ada")),
                ("seats", Some("3")),
                ("plan", None),
                ("meta", Some("{tier: 'gold'}")),
            ],
            &[
                ("_id".into(), "objectId".into()),
                ("name".into(), "string".into()),
                ("seats".into(), "int | null".into()),
                ("meta".into(), "object | string".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            statement,
            r#"db.getCollection("accounts").insertOne({"name": "Ada", "seats": 3, "plan": null, "meta": {"tier": "gold"}})"#
        );
        assert!(is_generated_write(&statement), "{statement}");
        assert!(insert_row("accounts", &[("name", Some(""))], &[]).is_err());
        assert!(
            insert_row(
                "accounts",
                &[("seats", Some("x"))],
                &[("seats".into(), "int".into())]
            )
            .is_err()
        );
        assert!(insert_row("accounts", &[("$where", Some("1"))], &[]).is_err());
    }

    #[test]
    fn a_delete_names_one_document_by_its_id() {
        let statement = delete_row(
            "we\"ird",
            &[("_id", "{a: NumberLong(1)}")],
            &[("_id".into(), "object".into())],
        )
        .unwrap();
        assert_eq!(
            statement,
            r#"db.getCollection("we\"ird").deleteOne({_id: {"a": NumberLong("1")}})"#
        );
        assert!(is_generated_write(&statement));
        assert!(delete_matches_key(&statement, &["_id"]));
        assert!(!delete_matches_key(&statement, &["id"]));
        assert!(!delete_matches_key(&statement, &["_id", "a"]));
        assert!(!delete_matches_key("db.c.deleteMany({_id: 1})", &["_id"]));
        assert!(delete_row("c", &[], &[]).is_err());
    }

    #[test]
    fn the_write_gate_admits_the_three_generated_shapes_and_nothing_else() {
        for admitted in [
            r#"db.getCollection("c").updateOne({_id: 1}, {$set: {"a": 1}})"#,
            r#"db.getCollection("c").updateOne({_id: 1}, {$set: {"a": 1}}); db.getCollection("c").updateOne({_id: "x"}, {$set: {b: {c: [1]}}});"#,
            r#"db.c.updateOne({_id: ObjectId('65a4f1c0ffffffffffffffff')}, {$set: {a: null}})"#,
            r#"db.getCollection("c").insertOne({"a": 1, b: {c: 2}})"#,
            r#"db.getCollection("c").deleteOne({_id: {a: 1}}) // why"#,
        ] {
            assert!(is_generated_write(admitted), "{admitted}");
        }
        for refused in [
            "",
            "db.c.find({_id: 1})",
            // An operator smuggled in through a value, at any depth.
            "db.c.updateOne({_id: 1}, {$set: {a: {$where: 'sleep(1000)'}}})",
            "db.c.updateOne({_id: 1}, {$set: {a: [{$gt: 1}]}})",
            "db.c.insertOne({a: {$where: 'x'}})",
            "db.c.insertOne({$where: 'x'})",
            "db.c.deleteOne({_id: {$ne: 1}})",
            "db.c.deleteOne({$where: 'true'})",
            // The filter has to be exactly `_id`, and a value only it equals.
            "db.c.deleteOne({_id: 1, a: 2})",
            "db.c.deleteOne({a: 1})",
            "db.c.deleteOne({})",
            "db.c.deleteOne({_id: /./})",
            "db.c.deleteOne({_id: null})",
            "db.c.deleteOne({_id: [1]})",
            "db.c.deleteOne({_id: ObjectId()})",
            "db.c.updateOne({_id: {$exists: true}}, {$set: {a: 1}})",
            "db.c.updateOne({a: 1}, {$set: {a: 1}})",
            // The update has to be exactly a `$set` of plain fields.
            "db.c.updateOne({_id: 1}, {$unset: {a: ''}})",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}, $inc: {b: 1}})",
            "db.c.updateOne({_id: 1}, {a: 1})",
            "db.c.updateOne({_id: 1}, [{$set: {a: 1}}])",
            "db.c.updateOne({_id: 1}, {$set: {}})",
            "db.c.updateOne({_id: 1}, {$set: {'a.b': 1}})",
            "db.c.updateOne({_id: 1}, {$set: {'$x': 1}})",
            "db.c.updateOne({_id: 1}, {$set: {_id: 2}})",
            "db.c.updateOne({_id: 1}, {$set: {a: 1, a: 2}})",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}}, {upsert: true})",
            // Other methods, other collections, other databases, more statements.
            "db.c.updateMany({_id: 1}, {$set: {a: 1}})",
            "db.c.replaceOne({_id: 1}, {a: 1})",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}}); db.d.updateOne({_id: 1}, {$set: {a: 1}})",
            "db.getSiblingDB('admin').c.updateOne({_id: 1}, {$set: {a: 1}})",
            "db.c.insertOne({a: 1}); db.c.insertOne({a: 2})",
            "db.c.insertOne({})",
            "db.c.insertOne({a: 1}, {writeConcern: {w: 0}})",
            "db.c.insertMany([{a: 1}])",
            "db.c.deleteOne({_id: 1}); db.c.deleteOne({_id: 2})",
            "db.c.deleteOne({_id: 1}); db.c.drop()",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}}); db.c.drop()",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}}); db.c.deleteOne({_id: 1})",
            "db.c.drop()",
            "db.dropDatabase()",
            "db.runCommand({delete: 'c', deletes: [{q: {}, limit: 0}]})",
            "show dbs",
            "db.c.updateOne({_id: 1}, {$set: {a: 1}}) db.c.drop()",
        ] {
            assert!(!is_generated_write(refused), "{refused} passed the gate");
        }
    }

    /// Format Query's answer for `text`, checked to read back as the same
    /// statements and to be its own answer.
    fn formatted(text: &str) -> String {
        let format = |text: &str| crate::sql::format(crate::db::Engine::MongoDb, text);
        let once = format(text).unwrap_or_else(|refusal| panic!("{text}: {refusal}"));
        assert_eq!(classify(&once), classify(text), "{once}");
        assert_eq!(statements(&once).len(), statements(text).len(), "{once}");
        assert_eq!(
            format(&once).as_deref(),
            Ok(once.as_str()),
            "not idempotent"
        );
        once
    }

    #[test]
    fn a_regex_comes_through_formatting_as_written() {
        // sqlformat made this `/ John Smith /`: another pattern.
        let once = formatted("db.users.find({name: /John  Smith/i})");
        assert_eq!(once, "db.users.find({ name: /John  Smith/i })");
    }

    #[test]
    fn a_comment_never_becomes_code_when_formatted() {
        // sqlformat broke both of these onto lines where the comment's text
        // ran: a live `db.c.drop()`, and a statement made of a note.
        let trailing = "db.c.find({}) // ; db.c.drop()";
        assert_eq!(formatted(trailing), trailing);
        let leading = "// note: drop later\ndb.c.find({})";
        assert_eq!(formatted(leading), leading);
    }

    #[test]
    fn a_long_statement_goes_one_item_per_line() {
        let once = formatted(
            r#"db.orders.aggregate([{$match:{status:"A"}},{$group:{_id:"$cust_id",total:{$sum:"$amount"}}},{$sort:{total:-1}}])"#,
        );
        assert_eq!(
            once,
            "db.orders.aggregate([\n  { $match: { status: \"A\" } },\n  \
             { $group: { _id: \"$cust_id\", total: { $sum: \"$amount\" } } },\n  \
             { $sort: { total: -1 } }\n])"
        );
        let once = formatted(
            "db.c.updateOne({ _id: 1 }, { $set: { name: 'x', tags: ['a', 'b', 'c'], nested: { deep: true } } })",
        );
        assert_eq!(
            once,
            "db.c.updateOne(\n  { _id: 1 },\n  \
             { $set: { name: 'x', tags: ['a', 'b', 'c'], nested: { deep: true } } }\n)"
        );
    }

    #[test]
    fn formatting_keeps_every_comment_and_whatever_stands_between_statements() {
        let text = "db.c.find({\n  a: 1, // the a\n      /* b */ b: 2,\n})\n.sort({ a: -1 })\n\n\
                    show dbs\n  db.c.insertOne({x: new Date(\"2024-01-01T00:00:00Z\")});db.c.countDocuments()";
        assert_eq!(
            formatted(text),
            "db.c.find({\n  a: 1, // the a\n  /* b */ b: 2,\n})\n  .sort({ a: -1 })\n\n\
             show dbs\n  db.c.insertOne({ x: new Date(\"2024-01-01T00:00:00Z\") });db.c.countDocuments()"
        );
    }

    #[test]
    fn a_half_typed_buffer_is_refused_rather_than_formatted() {
        let format = |text: &str| crate::sql::format(crate::db::Engine::MongoDb, text);
        for text in [
            "db.c.find({a:",
            "db.c.find({}).sort(",
            "db.c.insertOne({a: 1}",
        ] {
            let refusal = format(text).expect_err(text);
            assert!(!refusal.contains("dollar"), "{refusal}");
        }
        // Valid JavaScript, but not a statement DBDelve reads.
        assert!(format("db.c.find().count()").is_err());
    }
}
