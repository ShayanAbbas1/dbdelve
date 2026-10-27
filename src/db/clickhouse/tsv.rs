//! `TabSeparatedWithNamesAndTypes`, read the way the server wrote it: by the
//! column's type, and with a late exception told from data.
//!
//! Pure: it reads a `Read` and builds rows, and knows nothing of HTTP.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, BufRead, Read};
use std::sync::Arc;

use memchr::{memchr, memchr_iter, memmem};

use super::http::server_error;
use crate::db::{Column, DbError, RowBuilder, plain_error};

/// What `Reader` and `Framed` say when the body ended before the server meant
/// it to. Tests and the cancel logic tell it from a server's own error by
/// [`is_cut`].
pub const CUT: &str = "The server closed the response before it ended";

pub fn is_cut(error: &DbError) -> bool {
    error.message.starts_with(CUT)
}

/// Only the first 64 KiB of an unterminated exception is held back while the
/// reader decides whether it is one.
const HELD_LIMIT: usize = 64 * 1024;

/// A tagged frame is a message and a few bytes of trailer; nothing the server
/// sends there is longer than this.
const FRAME_LIMIT: u64 = 1024 * 1024;

const CHUNK: usize = 8192;

const FRAME_OPEN: &[u8] = b"\r\n__exception__\r\n";

/// How a field's bytes become text, decided once per column from its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unescape {
    /// `\t`, `\n`, `\\` and the rest of TSV's escapes are undone.
    Scalar { nullable: bool },
    /// ClickHouse literal text already: `['a\tb']` is shown as it arrived,
    /// because there is no TSV layer on top of it to undo.
    Raw { nullable: bool },
    /// A value whose active type is not in the text: an array, map or tuple
    /// arrives as literal text (`Raw`), anything else with its TSV escapes
    /// (`Scalar`).
    ByShape,
}

/// Everything a column's cells need, from its type string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoder {
    pub unescape: Unescape,
    /// `AggregateFunction(...)` is a binary state: hex, valid UTF-8 or not.
    pub binary: bool,
}

impl Decoder {
    pub fn of(data_type: &str) -> Self {
        let (base, nullable) = peel(data_type);
        let name = base.split('(').next().unwrap_or(base);
        let unescape = match name {
            "Array" | "Tuple" | "Map" | "Nested" | "Point" | "Ring" | "LineString"
            | "MultiLineString" | "Polygon" | "MultiPolygon" | "Object" => {
                Unescape::Raw { nullable }
            }
            // Which variant is active is not in the text, so it is told by the
            // first byte. `\N` is NULL.
            "Variant" | "Dynamic" => Unescape::ByShape,
            // JSON text, escaped once by JSON and once more by TSV.
            "JSON" => Unescape::Scalar { nullable: true },
            "Nothing" => Unescape::Scalar { nullable: true },
            _ => Unescape::Scalar { nullable },
        };
        Self {
            unescape,
            binary: name == "AggregateFunction",
        }
    }
}

/// The type under `Nullable`, `LowCardinality` (in either order) and
/// `SimpleAggregateFunction`, and whether a `Nullable` was among them.
pub(super) fn peel(data_type: &str) -> (&str, bool) {
    let mut current = data_type.trim();
    let mut nullable = false;
    loop {
        let inner = |prefix: &str| {
            current
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
        };
        if let Some(rest) = inner("Nullable(") {
            nullable = true;
            current = rest.trim();
        } else if let Some(rest) = inner("LowCardinality(") {
            current = rest.trim();
        } else if let Some(rest) = inner("SimpleAggregateFunction(") {
            match first_top_level_comma(rest) {
                Some(at) => current = rest[at + 1..].trim(),
                None => return (current, nullable),
            }
        } else {
            return (current, nullable);
        }
    }
}

fn first_top_level_comma(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (at, byte) in text.bytes().enumerate() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if quoted => escaped = true,
            b'\'' => quoted = !quoted,
            _ if quoted => {}
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => return Some(at),
            _ => {}
        }
    }
    None
}

/// The names and types lines as columns. Both are escaped like any field.
pub fn header(names: &[u8], types: &[u8]) -> Result<Vec<Column>, DbError> {
    let text = |bytes: &[u8]| -> Result<Vec<String>, DbError> {
        bytes
            .split(|byte| *byte == b'\t')
            .map(|field| {
                String::from_utf8(unescape(field).into_owned()).map_err(|_| {
                    plain_error("The server sent a column header that is not text.".into())
                })
            })
            .collect()
    };
    let names = text(names)?;
    let types = text(types)?;
    if names.len() != types.len() {
        return Err(plain_error(format!(
            "The server sent {} column names and {} column types.",
            names.len(),
            types.len()
        )));
    }
    Ok(names
        .into_iter()
        .zip(types)
        .map(|(name, data_type)| Column {
            name,
            data_type: Some(data_type),
        })
        .collect())
}

/// TSV's escapes undone; an unknown one keeps both its characters.
fn unescape(field: &[u8]) -> Cow<'_, [u8]> {
    let Some(first) = memchr(b'\\', field) else {
        return Cow::Borrowed(field);
    };
    let mut out = Vec::with_capacity(field.len());
    out.extend_from_slice(&field[..first]);
    let mut at = first;
    while at < field.len() {
        let byte = field[at];
        at += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&next) = field.get(at) else {
            out.push(b'\\');
            break;
        };
        at += 1;
        match next {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'r' => out.push(b'\r'),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'0' => out.push(0),
            b'\'' => out.push(b'\''),
            b'\\' => out.push(b'\\'),
            other => out.extend_from_slice(&[b'\\', other]),
        }
    }
    Cow::Owned(out)
}

/// A cell's text: the bytes as UTF-8, else `0x` and upper-case hex, the
/// spelling the other engines use for binary.
#[cfg(test)]
pub fn render<'a>(bytes: &'a [u8], data_type: &str) -> Cow<'a, str> {
    render_as(bytes, Decoder::of(data_type).binary)
}

fn render_as(bytes: &[u8], binary: bool) -> Cow<'_, str> {
    if !binary && let Ok(text) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(format!("0x{}", hex::encode_upper(bytes)))
}

/// A response body up to the tagged exception frame a server (25.11 and
/// later) appends to a 200 it can no longer turn into an error status:
/// `\r\n__exception__\r\n<TAG>\r\n<message>\r\n<len> <TAG>\r\n__exception__\r\n`.
///
/// The search is over the raw bytes, before any line splitting and across read
/// boundaries, with a tail of marker-length minus one held back so a marker
/// cut by a read still shows. The tag is 16 random bytes the server also sent
/// in a header, so it cannot be in the data by chance. Everything before the
/// frame is passed on; the frame itself reads as an error carrying a
/// [`DbError`], which the readers above turn back into one.
pub struct Framed<R> {
    inner: R,
    tagged: Option<Tagged>,
    buf: Vec<u8>,
    /// How much of the front of `buf` is cleared to hand out.
    ready: usize,
    state: Body,
}

struct Tagged {
    tag: Vec<u8>,
    marker: Vec<u8>,
    finder: memmem::Finder<'static>,
    statement: Arc<str>,
}

enum Body {
    Reading,
    /// The marker was found; the rest of the body is the frame.
    InFrame,
    Failed(DbError),
    Done,
}

impl<R: Read> Framed<R> {
    /// For a server that tags its responses; `statement` is what an error
    /// position is checked against.
    pub fn tagged(inner: R, tag: &str, statement: Arc<str>) -> Self {
        let marker = [FRAME_OPEN, tag.as_bytes()].concat();
        Self {
            inner,
            tagged: Some(Tagged {
                tag: tag.as_bytes().to_vec(),
                finder: memmem::Finder::new(&marker).into_owned(),
                marker,
                statement,
            }),
            buf: Vec::new(),
            ready: 0,
            state: Body::Reading,
        }
    }

    /// A body nothing marks: older servers, whose exceptions the line reader
    /// looks for instead.
    pub fn plain(inner: R) -> Self {
        Self {
            inner,
            tagged: None,
            buf: Vec::new(),
            ready: 0,
            state: Body::Reading,
        }
    }

    fn take_frame(&mut self) -> io::Result<()> {
        let tagged = self.tagged.as_ref().expect("a frame needs a tag");
        (&mut self.inner)
            .take(FRAME_LIMIT)
            .read_to_end(&mut self.buf)?;
        let error = parse_frame(&self.buf, tagged);
        self.buf.clear();
        self.state = Body::Failed(error);
        Ok(())
    }
}

impl<R: Read> Read for Framed<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.ready > 0 {
                let n = self.ready.min(out.len());
                out[..n].copy_from_slice(&self.buf[..n]);
                self.buf.drain(..n);
                self.ready -= n;
                return Ok(n);
            }
            match &self.state {
                Body::Done => return Ok(0),
                Body::Failed(error) => return Err(io::Error::other(error.clone())),
                Body::InFrame => self.take_frame()?,
                Body::Reading => {
                    let Some(tagged) = &self.tagged else {
                        return self.inner.read(out);
                    };
                    let held = self.buf.len();
                    self.buf.resize(held + CHUNK, 0);
                    let read = match self.inner.read(&mut self.buf[held..]) {
                        Ok(read) => read,
                        Err(error) => {
                            self.buf.truncate(held);
                            return Err(error);
                        }
                    };
                    self.buf.truncate(held + read);
                    if read == 0 {
                        self.ready = self.buf.len();
                        self.state = Body::Done;
                    } else if let Some(at) = tagged.finder.find(&self.buf) {
                        self.ready = at;
                        self.state = Body::InFrame;
                    } else {
                        self.ready = self.buf.len().saturating_sub(tagged.marker.len() - 1);
                    }
                }
            }
        }
    }
}

/// The frame, from its first byte to the end of the body, as the error it
/// reports. A frame whose trailer does not check out still reports the
/// message it did carry: the statement failed either way.
fn parse_frame(frame: &[u8], tagged: &Tagged) -> DbError {
    let garbled = |text: &[u8]| {
        let mut error = server_error(String::from_utf8_lossy(text).trim(), &tagged.statement);
        error.message = format!(
            "The server's error report was cut short or garbled: {}",
            error.message
        );
        error
    };
    let Some(body) = frame
        .strip_prefix(tagged.marker.as_slice())
        .and_then(|rest| rest.strip_prefix(b"\r\n".as_slice()))
    else {
        return garbled(frame);
    };
    let closing = [b" ".as_slice(), &tagged.tag, b"\r\n__exception__\r\n"].concat();
    let Some(before) = body.strip_suffix(closing.as_slice()) else {
        return garbled(body);
    };
    let Some(split) = memmem::rfind(before, b"\r\n") else {
        return garbled(body);
    };
    let (message, length) = (&before[..split], &before[split + 2..]);
    let counted = std::str::from_utf8(length)
        .ok()
        .and_then(|length| length.parse::<usize>().ok());
    if counted != Some(message.len()) {
        return garbled(message);
    }
    server_error(String::from_utf8_lossy(message).trim(), &tagged.statement)
}

/// What an older server (before the tagged frame) needs the line reader to
/// look for.
#[derive(Clone, Debug)]
pub struct Untagged {
    /// `version()`, as read at connect: the error text ends in it.
    pub version: String,
    pub statement: Arc<str>,
}

enum Mode {
    Tsv,
    /// One text column named for the format, a row per line.
    Foreign {
        format: String,
    },
}

/// What [`Reader::next_row`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// A row is in the builder.
    Row,
    /// An empty line in a result of more than one column: the rows that follow
    /// are a further set (`WITH TOTALS`, `extremes`).
    Section,
    End,
}

/// A body of rows, read a line at a time.
pub struct Reader<R: BufRead> {
    source: R,
    mode: Mode,
    columns: Vec<Column>,
    decoders: Vec<Decoder>,
    crlf: bool,
    untagged: Option<Untagged>,
    /// A line that may be the start of an untagged exception, and what
    /// followed it, until the next line shows it was data.
    held: Vec<Vec<u8>>,
    held_bytes: usize,
    /// Held lines that turned out to be data, to be read before the source.
    released: VecDeque<Vec<u8>>,
    raw: Vec<u8>,
    line: Vec<u8>,
}

impl<R: BufRead> Reader<R> {
    /// Reads the two header lines. An empty body is a command's, with no
    /// columns and no rows.
    pub fn open(source: R, untagged: Option<Untagged>) -> Result<Self, DbError> {
        let mut reader = Self {
            source,
            mode: Mode::Tsv,
            columns: Vec::new(),
            decoders: Vec::new(),
            crlf: false,
            untagged,
            held: Vec::new(),
            held_bytes: 0,
            released: VecDeque::new(),
            raw: Vec::new(),
            line: Vec::new(),
        };
        let Some(terminated) = reader.read_line()? else {
            return Ok(reader);
        };
        reader.crlf = reader.raw.ends_with(b"\r\n");
        if !terminated {
            return Err(cut("the column names"));
        }
        let mut names = std::mem::take(&mut reader.line);
        if reader.crlf {
            names.pop_if(|byte| *byte == b'\r');
        }
        if reader.read_line()?.is_none() {
            return Err(cut("the column types"));
        }
        reader.columns = header(&names, &reader.line)?;
        reader.decoders = reader
            .columns
            .iter()
            .map(|column| Decoder::of(column.data_type.as_deref().unwrap_or_default()))
            .collect();
        Ok(reader)
    }

    /// A statement that asked for a `FORMAT` of its own: its text, a row per
    /// line, in one column named for the format. Refused when the body is not
    /// text (Parquet, Native, RowBinary).
    pub fn foreign(source: R, format: &str) -> Self {
        Self {
            source,
            mode: Mode::Foreign {
                format: format.into(),
            },
            columns: vec![Column {
                name: format.into(),
                data_type: None,
            }],
            decoders: Vec::new(),
            crlf: false,
            untagged: None,
            held: Vec::new(),
            held_bytes: 0,
            released: VecDeque::new(),
            raw: Vec::new(),
            line: Vec::new(),
        }
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The next row's cells pushed onto `row`, which the caller finishes.
    pub fn next_row(&mut self, row: &mut RowBuilder) -> Result<Next, DbError> {
        if !self.pull()? {
            return Ok(Next::End);
        }
        let line = std::mem::take(&mut self.line);
        let result = self.decode(&line, row);
        self.line = line;
        result
    }

    fn decode(&self, line: &[u8], row: &mut RowBuilder) -> Result<Next, DbError> {
        if let Mode::Foreign { format } = &self.mode {
            if std::str::from_utf8(line).is_err() || line.contains(&0) {
                return Err(plain_error(format!(
                    "The statement asked for FORMAT {format}, which is not text."
                )));
            }
            row.push(Some(&String::from_utf8_lossy(line)));
            return Ok(Next::Row);
        }
        let width = self.columns.len();
        if width > 1 && line.is_empty() {
            return Ok(Next::Section);
        }
        let found = memchr_iter(b'\t', line).count() + 1;
        if found != width {
            return Err(plain_error(format!(
                "A row has {found} fields where the header has {width} columns."
            )));
        }
        for (field, decoder) in line.split(|byte| *byte == b'\t').zip(&self.decoders) {
            let (raw, nullable) = match decoder.unescape {
                Unescape::Scalar { nullable } => (false, nullable),
                Unescape::Raw { nullable } => (true, nullable),
                // A String that opens like a compound literal stays escaped.
                Unescape::ByShape => (matches!(field.first(), Some(b'[' | b'(' | b'{')), true),
            };
            if nullable && field == b"\\N" {
                row.push(None);
                continue;
            }
            let bytes = if raw {
                Cow::Borrowed(field)
            } else {
                unescape(field)
            };
            row.push(Some(&render_as(&bytes, decoder.binary)));
        }
        Ok(Next::Row)
    }

    /// Reads the rest of the set and counts its rows, stopping after the empty
    /// line that begins a further one ([`Next::Section`]) or at the end of the
    /// body ([`Next::End`]); `next_row` reads on from there. It goes through
    /// the same exception checks as `next_row`, so an error after the row
    /// limit fails the statement as one before it does.
    pub fn drain(&mut self) -> Result<(usize, Next), DbError> {
        let width = self.columns.len();
        if self.untagged.is_some() || !matches!(self.mode, Mode::Tsv) {
            return self.drain_lines();
        }
        let mut count = 0;
        let mut length = 0usize;
        let mut only_return = false;
        loop {
            let buf = self.source.fill_buf().map_err(read_failed)?;
            if buf.is_empty() {
                break;
            }
            let mut start = 0;
            let mut section = None;
            for end in memchr_iter(b'\n', buf) {
                let total = length + (end - start);
                let carried = length > 0;
                let empty = total == 0
                    || (self.crlf
                        && total == 1
                        && match carried {
                            true => only_return,
                            false => buf[start] == b'\r',
                        });
                if empty && width > 1 {
                    section = Some(end + 1);
                    break;
                }
                count += 1;
                start = end + 1;
                length = 0;
            }
            if let Some(through) = section {
                self.source.consume(through);
                return Ok((count, Next::Section));
            }
            if start < buf.len() {
                if length == 0 {
                    only_return = buf[start] == b'\r';
                }
                length += buf.len() - start;
            }
            let consumed = buf.len();
            self.source.consume(consumed);
        }
        if length > 0 {
            return Err(cut("a row"));
        }
        Ok((count, Next::End))
    }

    fn drain_lines(&mut self) -> Result<(usize, Next), DbError> {
        let width = self.columns.len();
        let mut count = 0;
        while self.pull()? {
            if matches!(self.mode, Mode::Tsv) && width > 1 && self.line.is_empty() {
                return Ok((count, Next::Section));
            }
            count += 1;
        }
        Ok((count, Next::End))
    }

    /// The next logical line into `self.line`, false at the end of the body.
    fn pull(&mut self) -> Result<bool, DbError> {
        loop {
            if let Some(line) = self.released.pop_front() {
                self.line = line;
                return Ok(true);
            }
            let Some(terminated) = self.read_line()? else {
                if self.held.is_empty() {
                    return Ok(false);
                }
                self.settle()?;
                continue;
            };
            if self.untagged.is_some() {
                let candidate = self.candidate(&self.line);
                if !self.held.is_empty() {
                    if self.fits(&self.line) {
                        let line = std::mem::take(&mut self.line);
                        self.release();
                        self.released.push_back(line);
                    } else {
                        self.hold(0);
                    }
                    continue;
                }
                if let Some(at) = candidate {
                    self.hold(at);
                    continue;
                }
            }
            if !terminated && matches!(self.mode, Mode::Tsv) {
                return Err(cut("a row"));
            }
            return Ok(true);
        }
    }

    /// Fills `self.line` without its terminator. `None` at the end of the
    /// body, otherwise whether the line was terminated.
    fn read_line(&mut self) -> Result<Option<bool>, DbError> {
        self.raw.clear();
        let read = self
            .source
            .read_until(b'\n', &mut self.raw)
            .map_err(read_failed)?;
        if read == 0 {
            return Ok(None);
        }
        let terminated = self.raw.last() == Some(&b'\n');
        let mut end = self.raw.len() - usize::from(terminated);
        if terminated && self.crlf && end > 0 && self.raw[end - 1] == b'\r' {
            end -= 1;
        }
        self.line.clear();
        self.line.extend_from_slice(&self.raw[..end]);
        Ok(Some(terminated))
    }

    /// Where in `line` an untagged exception may begin, when it is one that
    /// cannot be a row: in a one-column result at the start of the line; in a
    /// wider one anywhere in a line with the wrong number of fields.
    fn candidate(&self, line: &[u8]) -> Option<usize> {
        let at = exception_start(line)?;
        match self.columns.len() {
            1 => (at == 0).then_some(0),
            _ => (!self.fits(line)).then_some(at),
        }
    }

    /// Whether `line` can be a row: right field count, wider than one column.
    fn fits(&self, line: &[u8]) -> bool {
        let width = self.columns.len();
        width > 1 && memchr_iter(b'\t', line).count() + 1 == width
    }

    fn hold(&mut self, from: usize) {
        let line = self.line[from..].to_vec();
        self.held_bytes += line.len();
        self.held.push(line);
        if self.held_bytes > HELD_LIMIT {
            self.release();
        }
    }

    fn release(&mut self) {
        self.released.extend(self.held.drain(..));
        self.held_bytes = 0;
    }

    /// The body ended with lines held: an error if the last one closes with
    /// this server's version, else they were data.
    fn settle(&mut self) -> Result<(), DbError> {
        let untagged = self
            .untagged
            .as_ref()
            .expect("lines are held only untagged");
        let ends_the_message = self.held.last().is_some_and(|last| {
            let last = String::from_utf8_lossy(last);
            let last = last.trim_end();
            last.ends_with(')') && last.contains(&format!("(version {}", untagged.version))
        });
        if !ends_the_message {
            self.release();
            return Ok(());
        }
        let text = self
            .held
            .iter()
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
        Err(server_error(text.trim(), &untagged.statement))
    }
}

/// Where `Code: <digits>. DB::Exception: ` first occurs.
fn exception_start(line: &[u8]) -> Option<usize> {
    const SEPARATOR: &[u8] = b". DB::Exception: ";
    memmem::find_iter(line, b"Code: ").find(|&at| {
        let digits = line[at + 6..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        digits > 0 && line[at + 6 + digits..].starts_with(SEPARATOR)
    })
}

/// An error from the source: the frame's own, or a body that ended badly.
fn read_failed(error: io::Error) -> DbError {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DbError>())
    {
        Some(server) => server.clone(),
        None => plain_error(format!("{CUT}: {error}")),
    }
}

fn cut(what: &str) -> DbError {
    plain_error(format!("{CUT}, in the middle of {what}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn rows(reader: &mut Reader<impl BufRead>) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let mut builder = RowBuilder::default();
        let mut out = Vec::new();
        loop {
            match reader.next_row(&mut builder)? {
                Next::End => return Ok(out),
                Next::Section => out.push(vec![Some("--".into())]),
                Next::Row => {
                    let row = builder.finish()?;
                    out.push(
                        (0..row.len())
                            .map(|col| row.get(col).map(str::to_string))
                            .collect(),
                    );
                }
            }
        }
    }

    fn body(names: &str, types: &str, data: &[u8]) -> Vec<u8> {
        [names.as_bytes(), b"\n", types.as_bytes(), b"\n", data].concat()
    }

    fn read(names: &str, types: &str, data: &[u8]) -> Vec<Vec<Option<String>>> {
        let mut reader = Reader::open(Cursor::new(body(names, types, data)), None).unwrap();
        rows(&mut reader).unwrap()
    }

    fn one(types: &str, field: &[u8]) -> Option<String> {
        let data = [field, b"\n"].concat();
        read("c", types, &data).remove(0).remove(0)
    }

    fn text(value: &str) -> Option<String> {
        Some(value.into())
    }

    #[test]
    fn every_escape_is_undone_in_a_scalar() {
        assert_eq!(
            one("String", br"a\bb\fc\rd\ne\tf\0g\'h\\i"),
            text("a\u{8}b\u{c}c\rd\ne\tf\0g'h\\i")
        );
    }

    #[test]
    fn an_unknown_escape_keeps_both_characters() {
        assert_eq!(one("String", br"a\qb\"), text("a\\qb\\"));
    }

    #[test]
    fn null_is_the_two_bytes_only_where_the_type_is_nullable() {
        assert_eq!(one("Nullable(String)", br"\N"), None);
        assert_eq!(one("Nullable(String)", br"\\N"), text("\\N"));
        assert_eq!(one("String", br"\N"), text("\\N"));
        assert_eq!(one("Nullable(String)", b""), text(""));
    }

    #[test]
    fn an_empty_string_is_an_empty_cell() {
        assert_eq!(one("String", b""), text(""));
    }

    #[test]
    fn a_tab_and_a_newline_in_a_string_do_not_split_the_row() {
        let rows = read("a\tb", "String\tUInt8", b"x\\ty\\nz\t7\n");
        assert_eq!(rows, vec![vec![text("x\ty\nz"), text("7")]]);
    }

    #[test]
    fn composites_keep_their_own_escapes() {
        assert_eq!(
            one("Array(String)", br"['a\tb','it\'s','c\\d']"),
            text(r"['a\tb','it\'s','c\\d']")
        );
        assert_eq!(
            one("Tuple(String, UInt8)", br"('a\tb',1)"),
            text(r"('a\tb',1)")
        );
        assert_eq!(
            one("Map(String, String)", br"{'k\'':'v\\'}"),
            text(r"{'k\'':'v\\'}")
        );
        assert_eq!(
            one("Array(Nullable(String))", br"['a',NULL]"),
            text("['a',NULL]")
        );
    }

    #[test]
    fn wrappers_are_peeled_in_either_order() {
        assert_eq!(one("LowCardinality(Nullable(String))", br"\N"), None);
        assert_eq!(
            one("LowCardinality(Nullable(String))", br"a\tb"),
            text("a\tb")
        );
        assert_eq!(one("Nullable(LowCardinality(String))", br"\N"), None);
        assert_eq!(one("Nullable(Array(String))", br"\N"), None);
    }

    #[test]
    fn simple_aggregate_function_reads_as_its_type() {
        assert_eq!(
            one("SimpleAggregateFunction(anyLast, Nullable(String))", br"\N"),
            None
        );
        assert_eq!(
            one("SimpleAggregateFunction(sum, String)", br"a\tb"),
            text("a\tb")
        );
    }

    #[test]
    fn a_variant_is_read_by_its_shape_and_null_is_still_null() {
        assert_eq!(
            one("Variant(String, Array(String))", br"a\tb\\c"),
            text("a\tb\\c")
        );
        assert_eq!(
            one("Variant(String, Array(String))", br"['a\tb\\c']"),
            text(r"['a\tb\\c']")
        );
        assert_eq!(one("Dynamic", br"p\\q"), text(r"p\q"));
        assert_eq!(one("Variant(String, UInt8)", br"\N"), None);
        assert_eq!(one("Dynamic", br"\N"), None);
    }

    #[test]
    fn json_text_has_its_tsv_escapes_undone_once() {
        assert_eq!(
            one("JSON", br#"{"a":"b\\tc","d":"e\\\\f"}"#),
            text(r#"{"a":"b\tc","d":"e\\f"}"#)
        );
    }

    #[test]
    fn invalid_utf8_is_hex_and_valid_text_is_not() {
        assert_eq!(one("String", b"\xff\xfe"), text("0xFFFE"));
        assert_eq!(one("String", "héllo".as_bytes()), text("héllo"));
        assert_eq!(one("FixedString(3)", b"a\\0\\0"), text("a\0\0"));
    }

    #[test]
    fn an_aggregate_state_is_always_hex() {
        assert_eq!(
            one("AggregateFunction(uniq, UInt64)", b"abc"),
            text("0x616263")
        );
        assert_eq!(
            render(b"abc", "AggregateFunction(uniq, UInt64)"),
            "0x616263"
        );
        assert_eq!(render(b"abc", "String"), "abc");
    }

    #[test]
    fn the_type_line_is_unescaped_like_any_field() {
        let reader = Reader::open(Cursor::new(body("c", r"Enum8(\'a\' = 1)", b"")), None).unwrap();
        assert_eq!(
            reader.columns()[0].data_type.as_deref(),
            Some("Enum8('a' = 1)")
        );
    }

    #[test]
    fn crlf_is_read_from_the_first_line() {
        let crlf = b"a\tb\r\nString\tUInt8\r\nx\t1\r\ny\t2\r\n";
        let mut reader = Reader::open(Cursor::new(crlf.to_vec()), None).unwrap();
        assert_eq!(
            rows(&mut reader).unwrap(),
            vec![vec![text("x"), text("1")], vec![text("y"), text("2")]]
        );
        let mut reader = Reader::open(Cursor::new(crlf.to_vec()), None).unwrap();
        assert_eq!(reader.drain().unwrap(), (2, Next::End));
    }

    #[test]
    fn totals_and_extremes_are_further_sections_with_three_columns() {
        let data = b"a\t1\t2\nb\t3\t4\n\nx\t9\t9\n\nm\t0\t0\nM\t5\t5\n";
        let got = read("a\tb\tc", "String\tUInt8\tUInt8", data);
        assert_eq!(got.len(), 2 + 1 + 1 + 1 + 2);
        assert_eq!(got[2], vec![text("--")]);
        assert_eq!(got[4], vec![text("--")]);
    }

    /// The drain of a capped first set ends where the totals begin, so they
    /// and the extremes are still read as sets of their own.
    #[test]
    fn a_drain_stops_at_the_first_section_at_every_split_and_in_crlf() {
        let lf =
            b"a\tb\tc\nString\tUInt8\tUInt8\na\t1\t2\nb\t3\t4\n\nx\t9\t9\n\nm\t0\t0\n".to_vec();
        let crlf = String::from_utf8(lf.clone()).unwrap().replace('\n', "\r\n");
        for whole in [lf, crlf.into_bytes()] {
            for at in 1..whole.len() {
                let mut reader = tagged_reader(split_at(&whole, at));
                assert_eq!(reader.drain().unwrap(), (2, Next::Section), "split at {at}");
                let rest = rows(&mut reader).unwrap();
                assert_eq!(
                    rest,
                    [
                        vec![text("x"), text("9"), text("9")],
                        vec![text("--")],
                        vec![text("m"), text("0"), text("0")],
                    ],
                    "split at {at}"
                );
            }
        }
        let data = b"a\t1\nb\t2\n\nx\t9\n";
        let mut reader = Reader::open(
            Cursor::new(body("k\tn", "String\tUInt8", data)),
            untagged("26.8.1.1"),
        )
        .unwrap();
        assert_eq!(reader.drain().unwrap(), (2, Next::Section));
        assert_eq!(rows(&mut reader).unwrap(), [vec![text("x"), text("9")]]);
    }

    #[test]
    fn with_one_column_an_empty_line_is_an_empty_string_row() {
        let got = read("s", "String", b"a\n\nb\n");
        assert_eq!(got, vec![vec![text("a")], vec![text("")], vec![text("b")]]);
        let mut reader = Reader::open(Cursor::new(body("s", "String", b"a\n\nb\n")), None).unwrap();
        assert_eq!(reader.drain().unwrap(), (3, Next::End));
    }

    #[test]
    fn a_field_count_mismatch_is_an_error() {
        let mut reader =
            Reader::open(Cursor::new(body("a\tb", "String\tString", b"x\n")), None).unwrap();
        let error = rows(&mut reader).unwrap_err();
        assert!(error.message.contains("1 fields"), "{error}");
    }

    #[test]
    fn an_empty_body_is_a_command() {
        let mut reader = Reader::open(Cursor::new(Vec::new()), None).unwrap();
        assert!(reader.columns().is_empty());
        assert_eq!(
            rows(&mut reader).unwrap(),
            Vec::<Vec<Option<String>>>::new()
        );
    }

    #[test]
    fn a_body_ending_mid_row_is_cut() {
        let mut reader = Reader::open(Cursor::new(body("s", "String", b"a\nb")), None).unwrap();
        let error = rows(&mut reader).unwrap_err();
        assert!(is_cut(&error), "{error}");
        let mut reader = Reader::open(Cursor::new(body("s", "String", b"a\nb")), None).unwrap();
        assert!(is_cut(&reader.drain().unwrap_err()));
    }

    /// Hands out its parts one read at a time.
    struct Parts(VecDeque<Vec<u8>>);

    impl Read for Parts {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some(mut part) = self.0.pop_front() else {
                return Ok(0);
            };
            let n = part.len().min(out.len());
            out[..n].copy_from_slice(&part[..n]);
            if n < part.len() {
                self.0.push_front(part.split_off(n));
            }
            Ok(n)
        }
    }

    const TAG: &str = "0123456789abcdef";

    fn frame(message: &str) -> Vec<u8> {
        format!(
            "\r\n__exception__\r\n{TAG}\r\n{message}\r\n{} {TAG}\r\n__exception__\r\n",
            message.len()
        )
        .into_bytes()
    }

    fn split_at(bytes: &[u8], at: usize) -> Parts {
        Parts(VecDeque::from([bytes[..at].to_vec(), bytes[at..].to_vec()]))
    }

    fn tagged_reader(parts: Parts) -> Reader<BufReader<Framed<Parts>>> {
        let framed = Framed::tagged(parts, TAG, "SELECT 1".into());
        Reader::open(BufReader::new(framed), None).unwrap()
    }

    const FAILURE: &str = "Code: 395. DB::Exception: Value passed to 'throwIf' is not zero (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 26.8.1.1)";

    #[test]
    fn a_tagged_frame_after_rows_fails_at_every_split() {
        let whole = [b"s\nString\na\nb\n".as_slice(), &frame(FAILURE)].concat();
        for at in 1..whole.len() {
            let mut reader = tagged_reader(split_at(&whole, at));
            let error = rows(&mut reader).unwrap_err();
            assert!(
                error.message.contains("throwIf") && !error.message.starts_with("Code:"),
                "split at {at}: {error}"
            );
        }
    }

    #[test]
    fn the_rows_before_a_frame_are_still_delivered() {
        let whole = [b"s\nString\na\nb\n".as_slice(), &frame(FAILURE)].concat();
        let mut reader = tagged_reader(split_at(&whole, 5));
        let mut builder = RowBuilder::default();
        let mut seen = 0;
        let error = loop {
            match reader.next_row(&mut builder) {
                Ok(Next::Row) => {
                    builder.finish().unwrap();
                    seen += 1;
                }
                Ok(_) => panic!("ended without the error"),
                Err(error) => break error,
            }
        };
        assert_eq!(seen, 2);
        assert!(error.message.contains("throwIf"));
    }

    #[test]
    fn a_tagged_frame_before_any_row_fails_the_open() {
        let whole = frame(FAILURE);
        let framed = Framed::tagged(split_at(&whole, 7), TAG, "SELECT 1".into());
        let error = Reader::open(BufReader::new(framed), None).err().unwrap();
        assert!(error.message.contains("throwIf"), "{error}");
    }

    #[test]
    fn a_tagged_frame_fails_a_drain_at_every_split() {
        let whole = [b"s\nString\na\nb\nc\n".as_slice(), &frame(FAILURE)].concat();
        for at in 1..whole.len() {
            let mut reader = tagged_reader(split_at(&whole, at));
            let error = reader.drain().unwrap_err();
            assert!(error.message.contains("throwIf"), "split at {at}: {error}");
        }
    }

    #[test]
    fn a_drain_after_some_rows_were_read_still_counts_the_rest() {
        let whole = b"s\nString\na\nb\nc\nd\n".to_vec();
        let mut reader = tagged_reader(split_at(&whole, 9));
        let mut builder = RowBuilder::default();
        reader.next_row(&mut builder).unwrap();
        builder.finish().unwrap();
        assert_eq!(reader.drain().unwrap(), (3, Next::End));
    }

    #[test]
    fn a_frame_with_a_wrong_trailer_is_still_an_error() {
        let mut bad = frame("Code: 1. DB::Exception: oops (X) (version 1)");
        let at = bad.windows(2).rposition(|pair| pair == b"\r\n").unwrap();
        bad.truncate(at - 8);
        let whole = [b"s\nString\na\n".as_slice(), &bad].concat();
        let mut reader = tagged_reader(split_at(&whole, 12));
        let error = rows(&mut reader).unwrap_err();
        assert!(error.message.contains("garbled"), "{error}");
        assert!(error.message.contains("oops"), "{error}");

        let wrong_length = format!(
            "\r\n__exception__\r\n{TAG}\r\nCode: 1. DB::Exception: oops (X) (version 1)\r\n99 {TAG}\r\n__exception__\r\n"
        );
        let whole = [b"s\nString\n".as_slice(), wrong_length.as_bytes()].concat();
        let error = rows(&mut tagged_reader(split_at(&whole, 10))).unwrap_err();
        assert!(error.message.contains("garbled"), "{error}");
    }

    #[test]
    fn data_that_looks_like_a_frame_but_has_another_tag_is_data() {
        let lookalike = b"s\nString\n\\r\\n__exception__\\r\\n\n";
        let mut reader = tagged_reader(split_at(lookalike, 4));
        assert_eq!(rows(&mut reader).unwrap().len(), 1);
    }

    const OLD_FAILURE: &str = "Code: 395. DB::Exception: boom (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 25.8.1.5101 (official build))";

    fn untagged(version: &str) -> Option<Untagged> {
        Some(Untagged {
            version: version.into(),
            statement: "SELECT 1".into(),
        })
    }

    fn old(names: &str, types: &str, data: &str, version: &str) -> Result<usize, DbError> {
        let mut reader = Reader::open(
            Cursor::new(body(names, types, data.as_bytes())),
            untagged(version),
        )?;
        rows(&mut reader).map(|rows| rows.len())
    }

    #[test]
    fn an_untagged_exception_in_a_wider_result_is_found() {
        let data = format!("a\t1\nb\t2\n{OLD_FAILURE}\n");
        let error = old("a\tb", "String\tUInt8", &data, "25.8.1.5101").unwrap_err();
        assert!(error.message.contains("boom"), "{error}");
        let data = format!("a\t1\nb{OLD_FAILURE}\n");
        let error = old("a\tb", "String\tUInt8", &data, "25.8.1.5101").unwrap_err();
        assert!(error.message.contains("boom"), "{error}");
    }

    #[test]
    fn an_untagged_exception_in_a_one_column_result_is_found() {
        let data = format!("a\nb\n{OLD_FAILURE}\n");
        let error = old("s", "String", &data, "25.8.1.5101").unwrap_err();
        assert!(error.message.contains("boom"), "{error}");
    }

    #[test]
    fn an_untagged_exception_fails_a_drain() {
        let data = format!("a\nb\n{OLD_FAILURE}\n");
        let mut reader = Reader::open(
            Cursor::new(body("s", "String", data.as_bytes())),
            untagged("25.8.1.5101"),
        )
        .unwrap();
        assert!(reader.drain().unwrap_err().message.contains("boom"));
    }

    #[test]
    fn an_untagged_tail_for_another_version_is_data() {
        let data = format!("a\n{OLD_FAILURE}\nb\n");
        assert_eq!(old("s", "String", &data, "24.1.1.1").unwrap(), 3);
        let data = format!("a\n{OLD_FAILURE}\n");
        assert_eq!(old("s", "String", &data, "24.1.1.1").unwrap(), 2);
    }

    #[test]
    fn a_value_spelled_like_an_exception_in_the_middle_is_data() {
        let value = "Code: 1. DB::Exception: x (version 1)";
        let data = format!("{value}\t5\nb\t6\n");
        assert_eq!(old("a\tb", "String\tUInt8", &data, "1").unwrap(), 2);
    }

    #[test]
    fn a_foreign_format_is_one_text_column() {
        let body = br#"{"a":1}
{"a":2}
"#;
        let mut reader = Reader::foreign(Cursor::new(body.to_vec()), "JSONEachRow");
        assert_eq!(reader.columns()[0].name, "JSONEachRow");
        assert_eq!(
            rows(&mut reader).unwrap(),
            vec![vec![text(r#"{"a":1}"#)], vec![text(r#"{"a":2}"#)]]
        );
        let mut reader = Reader::foreign(Cursor::new(body.to_vec()), "JSONEachRow");
        assert_eq!(reader.drain().unwrap(), (2, Next::End));
    }

    #[test]
    fn a_foreign_body_with_a_frame_fails() {
        let whole = [b"{\"a\":1}\n".as_slice(), &frame(FAILURE)].concat();
        let framed = Framed::tagged(split_at(&whole, 9), TAG, "SELECT 1".into());
        let mut reader = Reader::foreign(BufReader::new(framed), "JSONEachRow");
        let error = rows(&mut reader).unwrap_err();
        assert!(error.message.contains("throwIf"), "{error}");
    }

    #[test]
    fn a_binary_foreign_body_is_refused() {
        let mut reader = Reader::foreign(Cursor::new(b"PAR1\xff\xfe\x00\n".to_vec()), "Parquet");
        let error = rows(&mut reader).unwrap_err();
        assert_eq!(
            error.message,
            "The statement asked for FORMAT Parquet, which is not text."
        );
    }

    /// Bodies recorded from the 26.8 dev container (`dev/clickhouse/fixtures`),
    /// with the headers that say how to read them.
    fn recorded(name: &str) -> (Vec<u8>, Option<String>) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/dev/clickhouse/fixtures");
        let body = std::fs::read(format!("{dir}/{name}.tsv")).unwrap();
        let headers = std::fs::read_to_string(format!("{dir}/{name}.headers")).unwrap();
        let tag = headers
            .lines()
            .find_map(|line| line.strip_prefix("X-ClickHouse-Exception-Tag: "))
            .map(str::to_string);
        (body, tag)
    }

    fn recorded_reader(name: &str) -> Reader<BufReader<Framed<Cursor<Vec<u8>>>>> {
        let (body, tag) = recorded(name);
        let framed = Framed::tagged(Cursor::new(body), &tag.unwrap(), "SELECT 1".into());
        Reader::open(BufReader::new(framed), None).unwrap()
    }

    #[test]
    fn the_recorded_type_gallery_reads_by_type() {
        let mut reader = recorded_reader("type_gallery");
        let columns: Vec<(String, String)> = reader
            .columns()
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone().unwrap()))
            .collect();
        let got = rows(&mut reader).unwrap();
        assert_eq!(got.len(), 2);
        let cell = |row: usize, name: &str| -> Option<&str> {
            let at = columns.iter().position(|(n, _)| n == name).unwrap();
            got[row][at].as_deref()
        };
        let type_of = |name: &str| columns.iter().find(|(n, _)| n == name).unwrap().1.as_str();
        assert_eq!(type_of("dec_small"), "Decimal(4, 1)");
        assert_eq!(type_of("moment_tz"), "DateTime('Asia/Tokyo')");
        assert_eq!(type_of("low_maybe"), "LowCardinality(Nullable(String))");
        assert_eq!(type_of("state"), "AggregateFunction(sum, UInt64)");

        for (name, first, second) in [
            (
                "u128",
                Some("340282366920938463463374607431768211455"),
                Some("0"),
            ),
            (
                "i256",
                Some(
                    "-57896044618658097711785492504343953926634992332820282019728792003956564819968",
                ),
                Some(
                    "57896044618658097711785492504343953926634992332820282019728792003956564819967",
                ),
            ),
            ("f32", Some("1.5"), Some("-inf")),
            ("f64", Some("0.1"), Some("nan")),
            ("dec_small", Some("-123.4"), Some("99.9")),
            (
                "dec_wide",
                Some("1234567890123456789012345678.123456789"),
                Some("-0.0000000001"),
            ),
            ("flag", Some("true"), Some("false")),
            ("day32", Some("1900-01-01"), Some("2299-12-31")),
            (
                "moment_tz",
                Some("2024-06-15 12:34:56"),
                Some("1970-01-01 09:00:00"),
            ),
            (
                "moment64",
                Some("2024-06-15 12:34:56.789"),
                Some("1970-01-01 00:00:00.001"),
            ),
            ("ip6", Some("2001:db8::1"), Some("::")),
            ("mood16", Some("high"), Some("low")),
            ("text", Some("tab\there 'quote' back\\slash"), Some("")),
            ("fixed", Some("ab\0\0"), Some("\0\0\0\0")),
            ("raw", Some("0x00FF00"), Some("")),
            ("maybe", None, Some("present")),
            ("low_maybe", None, Some("in\\ner")),
            (
                "strings",
                Some(r"['a\tb','it\'s','back\\slash']"),
                Some("[]"),
            ),
            ("maybes", Some("['x',NULL]"), Some("[]")),
            ("pair", Some(r"('t\tab',7)"), Some("('',0)")),
            ("labels", Some(r"{'k\tey':'v\'al','bs':'x\\y'}"), Some("{}")),
            ("point", Some("(1.5,-2.5)"), Some("(0,0)")),
            ("variant", Some("tab\there"), Some("42")),
            ("doc", Some(r#"{"a":[1,2,{}],"s":"t\tab"}"#), Some("{}")),
            ("simple", Some("7"), Some("-1")),
            (
                "state",
                Some("0x0500000000000000"),
                Some("0x0000000000000000"),
            ),
        ] {
            assert_eq!(cell(0, name), first, "row 1, {name}");
            assert_eq!(cell(1, name), second, "row 2, {name}");
        }
    }

    #[test]
    fn the_recorded_conformance_rows_read_as_the_suite_expects() {
        let got = rows(&mut recorded_reader("nulls_and_text")).unwrap();
        let cells: Vec<Vec<Option<&str>>> = got
            .iter()
            .map(|row| row.iter().map(|cell| cell.as_deref()).collect())
            .collect();
        assert_eq!(cells[0][5], Some("true"));
        assert_eq!(cells[0][3], Some("12.5"));
        assert_eq!(cells[0][8], Some("0x00FF"));
        assert_eq!(cells[1][1], Some("Zoë 李 🐉 'quoted'"));
        assert_eq!(cells[1][8], Some("0xDEADBEEF"));
        assert!(cells[2][2..].iter().all(Option::is_none));
    }

    #[test]
    fn the_recorded_totals_and_extremes_are_further_sections() {
        let got = rows(&mut recorded_reader("with_totals")).unwrap();
        let section = vec![Some("--".to_string())];
        let marks: Vec<usize> = (0..got.len()).filter(|&at| got[at] == section).collect();
        assert_eq!(marks, [6, 8]);
        assert_eq!(got[0][0].as_deref(), Some("churn"));
        assert_eq!(got[7], [text(""), text("1000000"), text("499995000")]);
        assert_eq!(got[9][0].as_deref(), Some("churn"));
        assert_eq!(got.len(), 11);
    }

    #[test]
    fn the_recorded_late_exception_fails_after_its_rows() {
        let mut reader = recorded_reader("late_exception");
        let mut builder = RowBuilder::default();
        let mut seen = 0;
        let error = loop {
            match reader.next_row(&mut builder) {
                Ok(Next::Row) => {
                    builder.finish().unwrap();
                    seen += 1;
                }
                Ok(_) => panic!("ended without the error"),
                Err(error) => break error,
            }
        };
        assert_eq!(seen, 40);
        assert!(
            error.message.contains("throwIf") && !error.message.starts_with("Code:"),
            "{error}"
        );
        assert!(
            error
                .message
                .contains("(FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)")
        );
    }
}
