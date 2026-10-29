//! Import Rows: reading a file, deciding what each value means, and writing it
//! into a table. This root holds the options the dialog edits; the rest of the
//! pipeline lives beside it.

use std::path::Path;

mod mapping;
mod plan;
mod read;
mod run;
mod values;

pub(crate) use mapping::*;
pub(crate) use plan::*;
pub(crate) use read::*;
pub(crate) use run::*;
pub(crate) use values::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Format {
    Csv,
    Tsv,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Delimiter {
    Auto,
    Comma,
    Semicolon,
    Tab,
    Pipe,
    Custom(char),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EmptyField {
    Null,
    EmptyString,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OnDuplicate {
    Stop,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Transaction {
    AllOrNothing,
    CommitAsItGoes,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Options {
    pub(crate) format: Format,
    pub(crate) header: bool,
    pub(crate) delimiter: Delimiter,
    pub(crate) quote: char,
    pub(crate) skip_lines: usize,
    pub(crate) encoding: Encoding,
    pub(crate) empty: EmptyField,
    pub(crate) null_word: String,
    pub(crate) trim: bool,
    pub(crate) booleans: Spellings,
    pub(crate) on_duplicate: OnDuplicate,
    pub(crate) transaction: Transaction,
}

impl Options {
    pub(crate) fn detected(path: &Path, bytes: &[u8]) -> Self {
        Self {
            format: detect(path, bytes),
            header: true,
            delimiter: Delimiter::Auto,
            quote: '"',
            skip_lines: 0,
            encoding: Encoding::sniff(bytes),
            empty: EmptyField::Null,
            null_word: String::new(),
            trim: false,
            booleans: Spellings::default(),
            on_duplicate: OnDuplicate::Stop,
            transaction: Transaction::AllOrNothing,
        }
    }
}

/// `row` indexes `Table::rows` and `column` is a file column index; either is
/// absent for a problem that belongs to the file or the line alone.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Problem {
    pub(crate) row: Option<usize>,
    pub(crate) line: Option<usize>,
    pub(crate) column: Option<usize>,
    pub(crate) message: String,
}
