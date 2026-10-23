//! The Import Rows dialog: how the file is read, which file column feeds which
//! table column, the rows and the SQL that will be sent, and the progress of
//! the run that sends them.

use std::collections::HashMap;

use gpui::{SharedString, relative};
use gpui_component::{
    FocusableExt, Sizable,
    checkbox::Checkbox,
    input::{self, Editor},
    table::{Column, DataTable, TableDelegate, TableState},
    tooltip::Tooltip,
};

use super::*;
use crate::import_rows::{
    Checked, DATE_FORMATS, DateFormat, Decimal, Delimiter, EmptyField, Encoding, Format, Mapping,
    OnDuplicate, Options, Prepared, Problem, Rule, Spellings, Target, TargetColumn, Transaction,
    Unmapped, offered_rules, reassign, unmapped,
};
use crate::result_grid::NULL_LABEL;
use crate::session::{ImportDialog, ImportPhase};

/// The rows the preview holds at once. Validation still covers the whole file.
pub(crate) const PREVIEW_ROWS: usize = 100;
/// Rows kept above a jumped-to problem, so it is read in its place.
pub(crate) const PREVIEW_CONTEXT: usize = 10;
const OPTIONS_WIDTH: f32 = 232.0;
const MAPPING_MIN_WIDTH: f32 = 300.0;
const LINE_COLUMN_WIDTH: f32 = 64.0;
const VALUE_COLUMN_WIDTH: f32 = 140.0;
const DELIMITER_INPUT_WIDTH: f32 = 44.0;

/// The first row the preview shows so that `problem_row` sits under
/// `PREVIEW_CONTEXT` rows of its own, with a full window wherever one fits.
pub(crate) fn preview_window(problem_row: usize, rows: usize) -> usize {
    problem_row
        .saturating_sub(PREVIEW_CONTEXT)
        .min(rows.saturating_sub(PREVIEW_ROWS))
}

/// The preview's rows: up to `PREVIEW_ROWS` of the checked file from `start`,
/// headed by a faint Line column and the columns the insert names.
pub(crate) struct PreviewGrid {
    checked: Arc<Checked>,
    start: usize,
    /// By file row and grid column, where column 0 is Line: a problem with
    /// no column of its own, or on a file column left out, marks the line.
    problems: HashMap<(usize, usize), String>,
}

impl PreviewGrid {
    fn new(checked: Arc<Checked>, start: usize) -> Self {
        let shown = start..(start + PREVIEW_ROWS).min(checked.rows.len());
        let mut problems = HashMap::new();
        for problem in &checked.problems {
            let Some(row) = problem.row.filter(|row| shown.contains(row)) else {
                continue;
            };
            let column = problem
                .column
                .and_then(|file| checked.sources.iter().position(|&source| source == file))
                .map_or(0, |k| k + 1);
            problems
                .entry((row, column))
                .or_insert_with(|| problem.message.clone());
        }
        Self {
            checked,
            start,
            problems,
        }
    }

    fn text(&self, row_ix: usize, col_ix: usize) -> Option<SharedString> {
        let row = self.checked.rows.get(self.start + row_ix)?;
        match col_ix {
            0 => Some(group_thousands(row.line as u64).into()),
            k => row
                .values
                .get(k - 1)?
                .as_ref()
                .map(|value| value.shown().map_or(NULL_LABEL, SharedString::from)),
        }
    }
}

pub(crate) fn new_preview(
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<PreviewGrid>> {
    let empty = Arc::new(Checked {
        columns: Vec::new(),
        sources: Vec::new(),
        rows: Vec::new(),
        problems: Vec::new(),
    });
    cx.new(|cx| {
        TableState::new(PreviewGrid::new(empty, 0), window, cx)
            .col_movable(false)
            .col_resizable(true)
            .sortable(false)
            .col_selectable(false)
            .row_selectable(false)
            .cell_selectable(false)
            .row_header(false)
    })
}

/// Put the newest check's rows in the preview, from where it was as far as
/// the file still reaches. Changed in place, so the last rows stay on screen
/// until these replace them.
pub(crate) fn show_preview(dialog: &mut ImportDialog, cx: &mut App) {
    let Some(Ok(prepared)) = &dialog.prepared else {
        return;
    };
    let checked = prepared.checked.clone();
    dialog.preview_start = dialog
        .preview_start
        .min(checked.rows.len().saturating_sub(PREVIEW_ROWS));
    let grid = PreviewGrid::new(checked, dialog.preview_start);
    dialog.preview.update(cx, |table, cx| {
        *table.delegate_mut() = grid;
        table.refresh(cx);
    });
}

/// Move the preview to the rows around `row` and bring it to the top.
pub(crate) fn show_preview_row(dialog: &mut ImportDialog, row: usize, cx: &mut App) {
    let Some(Ok(prepared)) = &dialog.prepared else {
        return;
    };
    let start = preview_window(row, prepared.checked.rows.len());
    dialog.preview_start = start;
    let grid = PreviewGrid::new(prepared.checked.clone(), start);
    dialog.preview.update(cx, |table, cx| {
        *table.delegate_mut() = grid;
        table.refresh(cx);
        table.scroll_to_row(row - start, cx);
    });
}

impl TableDelegate for PreviewGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.checked.columns.len() + 1
    }

    fn rows_count(&self, _: &App) -> usize {
        self.checked
            .rows
            .len()
            .saturating_sub(self.start)
            .min(PREVIEW_ROWS)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("line", "Line")
                .width(px(LINE_COLUMN_WIDTH))
                .resizable(false),
            k => Column::new(
                SharedString::from(format!("column-{k}")),
                self.checked.columns[k - 1].0.clone(),
            )
            .width(px(VALUE_COLUMN_WIDTH))
            .min_width(px(60.))
            .max_width(px(f32::MAX))
            .resizable(true),
        }
        .movable(false)
        .p_0()
    }

    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }

    /// Dressed as `StructureGrid::render_th`, with Line as faint as its cells.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = popup_theme(cx);
        let name: SharedString = match col_ix {
            0 => "Line".into(),
            k => self.checked.columns[k - 1].0.clone().into(),
        };
        div()
            .h_full()
            .flex_1()
            .min_w_0()
            .pl(px(layout::SPACE_SM + 1.))
            .pr(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(layout::grid(layout::TEXT_SM)))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if col_ix == 0 { t.text_faint } else { t.text })
            .child(name)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = popup_theme(cx);
        let text = self.text(row_ix, col_ix);
        let null = col_ix > 0 && text.as_ref() == Some(&NULL_LABEL);
        let problem = self.problems.get(&(self.start + row_ix, col_ix)).cloned();
        div()
            .id((
                "import-preview-cell",
                row_ix * (self.checked.columns.len() + 1) + col_ix,
            ))
            .relative()
            .size_full()
            .px(px(layout::SPACE_SM))
            // Transparent, so text sits where the other grids put it.
            .border_1()
            .flex()
            .items_center()
            .child(
                div()
                    .absolute()
                    .top(px(-1.))
                    .bottom(px(-1.))
                    .right(px(-1.))
                    .w(px(1.))
                    .bg(t.border),
            )
            .when_some(problem, |cell, message| {
                cell.bg(t.danger.alpha(0.16))
                    .tooltip(move |window, cx| Tooltip::new(message.clone()).build(window, cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if col_ix == 0 || null {
                        t.text_faint
                    } else {
                        t.text
                    })
                    // Italic, as in the result grid, so a NULL cannot be
                    // mistaken for the four-letter string.
                    .when(null, |cell| cell.italic())
                    .children(text),
            )
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.text(row_ix, col_ix)
            .map(|text| text.to_string())
            .unwrap_or_default()
    }
}

/// What every control in the dialog reads, and whose dialog it writes.
struct Editing<'a> {
    profile: String,
    dialog: &'a ImportDialog,
    engine: Engine,
    skips_duplicates: bool,
    running: bool,
}

impl Workspace {
    pub(crate) fn render_import_rows(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let dialog = profile.session.import_rows.as_ref()?;
        let engine = profile.config.engine();
        let targets = Rc::new(match &dialog.structure {
            StructureState::Loaded(structure) => TargetColumn::from_structure(structure, engine),
            _ => Vec::new(),
        });
        let skips_duplicates = match &dialog.structure {
            StructureState::Loaded(structure) => engine.skips_duplicates_into(structure),
            _ => engine.skips_duplicates(),
        };
        let editing = Editing {
            profile: profile.id.clone(),
            dialog,
            engine,
            skips_duplicates,
            running: matches!(dialog.phase, ImportPhase::Running { .. }),
        };
        let body = self.import_body(
            &editing,
            &targets,
            matches!(dialog.structure, StructureState::Loading),
            cx,
        );
        let footer = match &dialog.phase {
            ImportPhase::Editing => import_footer(&profile.id, dialog, cx),
            ImportPhase::Running { .. } => import_progress(dialog, cx),
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog_card(t)
                        .child(import_header(dialog, cx))
                        .child(body)
                        .child(footer),
                )
                .into_any_element(),
        )
    }

    /// Options on the left; then the mapping and the preview, or what stands
    /// in for them while there are none.
    fn import_body(
        &self,
        editing: &Editing,
        targets: &Rc<Vec<TargetColumn>>,
        loading: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = popup_theme(cx);
        let dialog = editing.dialog;
        let options = frozen(
            div()
                .id("import-options")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_LG))
                .pr(px(layout::SPACE_XS))
                .children(self.import_options(editing, cx)),
            editing.running,
        )
        .w(px(OPTIONS_WIDTH))
        .flex_shrink_0();

        let rest = match &dialog.prepared {
            Some(Ok(prepared)) => div()
                .flex_1()
                .min_w_0()
                .flex()
                .gap(px(layout::SPACE_LG))
                .child(
                    frozen(
                        import_mapping(editing, prepared, targets, cx),
                        editing.running,
                    )
                    .flex_1()
                    .min_w(px(MAPPING_MIN_WIDTH)),
                )
                .child(import_preview(editing, cx))
                .into_any_element(),
            Some(Err(message)) => div()
                .id("import-error-body")
                .flex_1()
                .overflow_y_scroll()
                .text_color(t.danger)
                .children(lines(message))
                .into_any_element(),
            None if loading => div()
                .flex_1()
                .text_color(t.text_muted)
                .child(format!(
                    "Reading the columns of `{}.{}`…",
                    dialog.schema, dialog.table
                ))
                .into_any_element(),
            None => div().flex_1().into_any_element(),
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .gap(px(layout::SPACE_LG))
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .child(options)
            .child(rest)
            .into_any_element()
    }

    /// The options, a group per section, each read from the dialog and
    /// written back through `edit_import`.
    fn import_options(&self, editing: &Editing, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = popup_theme(cx);
        let dialog = editing.dialog;
        let options = &dialog.options;
        let mut groups = Vec::new();

        let format = segments(
            editing,
            "import-format",
            &[
                (Format::Csv, "CSV"),
                (Format::Tsv, "TSV"),
                (Format::Json, "JSON"),
            ],
            options.format,
            |options, format| options.format = format,
            cx,
        );
        groups.push(group(t, "Format", vec![format.into_any_element()]));

        // Skip first N lines among them: a JSON file has no preamble to skip.
        if options.format != Format::Json {
            let header = checkbox(
                editing,
                "import-header",
                "First row is a header",
                options.header,
                |options, on| options.header = on,
                cx,
            );
            let custom = matches!(options.delimiter, Delimiter::Custom(_));
            let delimiter = self
                .labelled_field(
                    "Delimiter",
                    div()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(delimiter_dropdown(editing, cx)),
                        )
                        .children(custom.then(|| {
                            div().w(px(DELIMITER_INPUT_WIDTH)).child(
                                Input::new(&dialog.inputs.delimiter)
                                    .w_full()
                                    .disabled(editing.running),
                            )
                        })),
                    cx,
                )
                .into_any_element();
            let quote = self
                .labelled_field(
                    "Quote",
                    option_dropdown(
                        editing,
                        "import-quote",
                        options.quote,
                        vec!['"', '\''],
                        |quote| quote.to_string().into(),
                        |options, quote| options.quote = quote,
                        cx,
                    ),
                    cx,
                )
                .into_any_element();
            let skip = self
                .labelled_field(
                    "Skip first N lines",
                    Input::new(&dialog.inputs.skip_lines)
                        .w_full()
                        .disabled(editing.running),
                    cx,
                )
                .into_any_element();
            let encoding = self
                .labelled_field(
                    "Encoding",
                    option_dropdown(
                        editing,
                        "import-encoding",
                        options.encoding,
                        vec![
                            Encoding::Utf8,
                            Encoding::Utf16Le,
                            Encoding::Utf16Be,
                            Encoding::Windows1252,
                        ],
                        |encoding| {
                            match encoding {
                                Encoding::Utf8 => "UTF-8",
                                Encoding::Utf16Le => "UTF-16 LE",
                                Encoding::Utf16Be => "UTF-16 BE",
                                Encoding::Windows1252 => "Windows-1252",
                            }
                            .into()
                        },
                        |options, encoding| options.encoding = encoding,
                        cx,
                    ),
                    cx,
                )
                .into_any_element();
            groups.push(group(
                t,
                "CSV options",
                vec![header, delimiter, quote, skip, encoding],
            ));
        }

        let empty = segments(
            editing,
            "import-empty",
            &[
                (EmptyField::Null, "NULL"),
                (EmptyField::EmptyString, "Empty string"),
            ],
            options.empty,
            |options, empty| options.empty = empty,
            cx,
        );
        let mut values = vec![
            self.labelled_field("Empty field means", empty, cx)
                .into_any_element(),
            self.labelled_field(
                "Also treat as NULL",
                Input::new(&dialog.inputs.null_word)
                    .w_full()
                    .disabled(editing.running),
                cx,
            )
            .into_any_element(),
            checkbox(
                editing,
                "import-trim",
                "Trim whitespace",
                options.trim,
                |options, on| options.trim = on,
                cx,
            ),
        ];
        let parses_booleans = dialog
            .mappings
            .iter()
            .any(|mapping| mapping.rule == Rule::Boolean && mapping.target != Target::Skip);
        if parses_booleans {
            // The last spellings that parsed stay in force while this reads.
            let refused = Spellings::parse(&dialog.inputs.booleans.read(cx).value()).err();
            let spellings = div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    Input::new(&dialog.inputs.booleans)
                        .w_full()
                        .disabled(editing.running),
                )
                .children(refused.map(|why| div().text_color(t.danger).child(why)));
            values.push(
                self.labelled_field("True / false spellings", spellings, cx)
                    .into_any_element(),
            );
        }
        groups.push(group(t, "Values", values));

        if editing.skips_duplicates {
            let mut duplicate = vec![
                segments(
                    editing,
                    "import-duplicate",
                    &[(OnDuplicate::Stop, "Stop"), (OnDuplicate::Skip, "Skip")],
                    options.on_duplicate,
                    |options, on_duplicate| options.on_duplicate = on_duplicate,
                    cx,
                )
                .into_any_element(),
            ];
            if options.on_duplicate == OnDuplicate::Skip {
                duplicate.push(help(
                    t,
                    "Skips a row that conflicts with an existing row on any unique key, not only the primary key.",
                ));
            }
            groups.push(group(t, "On duplicate key", duplicate));
        }

        let transaction = match editing.engine.all_or_nothing() {
            Ok(_) => vec![
                segments(
                    editing,
                    "import-transaction",
                    &[
                        (Transaction::AllOrNothing, "All or nothing"),
                        (Transaction::CommitAsItGoes, "Commit as it goes"),
                    ],
                    options.transaction,
                    |options, transaction| options.transaction = transaction,
                    cx,
                )
                .into_any_element(),
                help(
                    t,
                    match options.transaction {
                        Transaction::AllOrNothing => {
                            "Every chunk runs in one transaction: a failure or Cancel leaves the table as it was."
                        }
                        Transaction::CommitAsItGoes => {
                            "Each chunk commits on its own: if a later chunk fails, the rows already committed stay."
                        }
                    },
                ),
            ],
            Err(why) => vec![
                div().child("Commit as it goes").into_any_element(),
                help(t, why),
            ],
        };
        groups.push(group(t, "Transaction", transaction));
        groups
    }
}

fn dialog_card(t: Theme) -> gpui::Div {
    dialog(t)
        .w(px(layout::IMPORT_DIALOG_WIDTH))
        .h(px(layout::IMPORT_DIALOG_HEIGHT))
        .max_w(relative(0.95))
        .max_h(relative(0.92))
}

/// A column nobody can change while the import runs: dimmed, and covered so
/// no click reaches a control in it.
fn frozen(column: impl IntoElement, running: bool) -> gpui::Div {
    div()
        .relative()
        .min_h_0()
        .flex()
        .flex_col()
        .child(column)
        .when(running, |column| {
            column
                .opacity(0.5)
                .child(div().absolute().inset_0().occlude())
        })
}

fn group(t: Theme, label: &str, controls: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_SM))
        .child(section_label(t, label))
        .children(controls)
        .into_any_element()
}

fn help(t: Theme, text: &'static str) -> AnyElement {
    div()
        .text_color(t.text_faint)
        .child(text)
        .into_any_element()
}

/// A segmented choice of one option. The chosen one takes no click, so it
/// reads nothing again.
fn segments<T: Copy + PartialEq + 'static>(
    editing: &Editing,
    id: &'static str,
    choices: &[(T, &'static str)],
    selected: T,
    write: fn(&mut Options, T),
    cx: &mut Context<Workspace>,
) -> gpui::Div {
    let t = popup_theme(cx);
    div()
        .flex()
        .flex_wrap()
        .gap(px(layout::SPACE_XS))
        .children(choices.iter().enumerate().map(|(index, &(value, label))| {
            let profile = editing.profile.clone();
            segment((id, index), label, value == selected, t).when(value != selected, |segment| {
                segment.on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                    workspace.edit_import(
                        &profile,
                        |dialog| {
                            write(&mut dialog.options, value);
                            true
                        },
                        cx,
                    );
                }))
            })
        }))
}

/// Dressed as the connection form's.
fn checkbox(
    editing: &Editing,
    id: &'static str,
    label: &'static str,
    checked: bool,
    write: fn(&mut Options, bool),
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let profile = editing.profile.clone();
    Checkbox::new(id)
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .font_weight(FontWeight::MEDIUM)
        .label(label)
        .checked(checked)
        .disabled(editing.running)
        .on_click(cx.listener(move |workspace, on: &bool, _, cx| {
            let on = *on;
            workspace.edit_import(
                &profile,
                |dialog| {
                    write(&mut dialog.options, on);
                    true
                },
                cx,
            );
        }))
        .into_any_element()
}

fn option_dropdown<T: Clone + PartialEq + 'static>(
    editing: &Editing,
    id: &'static str,
    selected: T,
    choices: Vec<T>,
    label: impl Fn(&T) -> SharedString + 'static,
    write: fn(&mut Options, T),
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let profile = editing.profile.clone();
    Workspace::dropdown(
        id,
        selected,
        vec![choices],
        label,
        |_| None,
        move |workspace, value, _, cx| {
            workspace.edit_import(
                &profile,
                |dialog| {
                    write(&mut dialog.options, value);
                    true
                },
                cx,
            );
        },
        cx,
    )
}

fn delimiter_name(delimiter: char) -> String {
    match delimiter {
        ',' => "comma".into(),
        ';' => "semicolon".into(),
        '\t' => "tab".into(),
        '|' => "pipe".into(),
        other => other.to_string(),
    }
}

fn delimiter_dropdown(editing: &Editing, cx: &mut Context<Workspace>) -> AnyElement {
    let dialog = editing.dialog;
    let read_with = match &dialog.prepared {
        Some(Ok(prepared)) => prepared.delimiter,
        _ => None,
    };
    // One Custom entry, carrying the character in force when it is the one
    // chosen, so the menu marks it.
    let custom = match dialog.options.delimiter {
        Delimiter::Custom(c) => Delimiter::Custom(c),
        _ => Delimiter::Custom(read_with.unwrap_or(',')),
    };
    let profile = editing.profile.clone();
    let input = dialog.inputs.delimiter.clone();
    Workspace::dropdown(
        "import-delimiter",
        dialog.options.delimiter,
        vec![
            vec![
                Delimiter::Auto,
                Delimiter::Comma,
                Delimiter::Semicolon,
                Delimiter::Tab,
                Delimiter::Pipe,
            ],
            vec![custom],
        ],
        move |delimiter| match delimiter {
            Delimiter::Auto => match read_with {
                Some(c) => format!("Auto ({})", delimiter_name(c)).into(),
                None => "Auto".into(),
            },
            Delimiter::Comma => "Comma".into(),
            Delimiter::Semicolon => "Semicolon".into(),
            Delimiter::Tab => "Tab".into(),
            Delimiter::Pipe => "Pipe".into(),
            Delimiter::Custom(_) => "Custom…".into(),
        },
        |_| None,
        move |workspace, delimiter, window, cx| {
            // Custom starts from what its field holds, else from the
            // character the file was last read with, which the field then
            // shows.
            let delimiter = match delimiter {
                Delimiter::Custom(seed) => {
                    let typed = input.read(cx).value().chars().next();
                    if typed.is_none() {
                        input.update(cx, |input, cx| {
                            input.set_value(seed.to_string(), window, cx);
                        });
                    }
                    Delimiter::Custom(typed.unwrap_or(seed))
                }
                other => other,
            };
            workspace.edit_import(
                &profile,
                |dialog| {
                    let changed = dialog.options.delimiter != delimiter;
                    dialog.options.delimiter = delimiter;
                    changed
                },
                cx,
            );
        },
        cx,
    )
}

/// A rule as the one `offered_rules` stands for its kind, so a chosen date
/// format or decimal separator still marks its kind in the menu.
fn kind(rule: &Rule) -> Rule {
    match rule {
        Rule::Date(_) => Rule::Date(DateFormat::Iso),
        Rule::Number(_) => Rule::Number(Decimal::Dot),
        other => other.clone(),
    }
}

fn rule_label(rule: &Rule) -> SharedString {
    match rule {
        Rule::AsIs => "As is",
        Rule::Date(_) => "Date / time",
        Rule::Number(_) => "Number",
        Rule::Boolean => "Boolean",
        Rule::Exact => "Text, kept exactly",
    }
    .into()
}

fn target_column(targets: &[TargetColumn], target: Target) -> Option<&TargetColumn> {
    match target {
        Target::Column(i) => targets.get(i),
        Target::NewField | Target::Skip => None,
    }
}

fn import_mapping(
    editing: &Editing,
    prepared: &Prepared,
    targets: &Rc<Vec<TargetColumn>>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let t = popup_theme(cx);
    let dialog = editing.dialog;
    // Read from the dialog's own mappings, which an edit changes at once,
    // over the columns of the check they came from.
    let rows: Vec<AnyElement> = prepared
        .columns
        .iter()
        .zip(&dialog.mappings)
        .enumerate()
        .map(|(i, (name, mapping))| {
            let sample = prepared.samples.get(i).cloned().flatten();
            mapping_row(editing, i, name, sample, mapping, targets, cx)
        })
        .collect();

    let left = unmapped(targets, &dialog.mappings, editing.engine);
    let unmapped_list = (!left.is_empty()).then(|| {
        div()
            .pt(px(layout::SPACE_SM))
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .child(section_label(t, "Unmapped table columns"))
            .children(left.into_iter().map(|(i, what)| {
                let status = match what {
                    Unmapped::UsesDefault => "uses default",
                    Unmapped::Null => "NULL",
                    Unmapped::Generated => "generated",
                    Unmapped::LeftOut => "left out",
                    Unmapped::Required => "NOT NULL, no default",
                };
                let required = what == Unmapped::Required;
                let profile = editing.profile.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(targets[i].name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(if required { t.danger } else { t.text_faint })
                            .child(status),
                    )
                    .children(required.then(|| {
                        button(
                            ("map-a-column", i),
                            "Map a column",
                            Tone::Quiet,
                            Control::Inline,
                            t,
                        )
                        .on_click(cx.listener(
                            move |workspace, _, _, cx| {
                                workspace.focus_import_mapping(&profile, 0, cx);
                            },
                        ))
                    }))
            }))
    });

    div()
        .id("import-mapping")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .track_scroll(&dialog.mapping_scroll)
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .children(rows)
        .children(unmapped_list)
}

fn mapping_row(
    editing: &Editing,
    i: usize,
    name: &str,
    sample: Option<String>,
    mapping: &Mapping,
    targets: &Rc<Vec<TargetColumn>>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = popup_theme(cx);
    let focused = editing.dialog.focused_mapping == Some(i);

    let mut choices = vec![
        targets
            .iter()
            .enumerate()
            .filter(|(_, column)| !column.generated)
            .map(|(k, _)| Target::Column(k))
            .collect::<Vec<_>>(),
    ];
    if editing.engine.takes_new_fields() {
        choices.push(vec![Target::NewField]);
    }
    choices.push(vec![Target::Skip]);
    choices.retain(|group| !group.is_empty());
    let target_dropdown = {
        let (labels, offered) = (targets.clone(), targets.clone());
        let file_column = name.to_string();
        let profile = editing.profile.clone();
        Workspace::dropdown(
            ("import-target", i),
            mapping.target,
            choices,
            move |choice| match *choice {
                Target::Column(k) => labels
                    .get(k)
                    .map_or_else(SharedString::default, |column| column.name.clone().into()),
                Target::NewField => format!("New field \"{file_column}\"").into(),
                Target::Skip => "Skip".into(),
            },
            |_| None,
            move |workspace, choice, _, cx| {
                let rules = offered_rules(target_column(&offered, choice));
                workspace.edit_import(
                    &profile,
                    |dialog| {
                        if dialog.mappings.get(i).is_none_or(|m| m.target == choice) {
                            return false;
                        }
                        reassign(&mut dialog.mappings, i, choice);
                        // A rule the new target does not offer goes back to As is.
                        let mapping = &mut dialog.mappings[i];
                        if !rules.contains(&kind(&mapping.rule)) {
                            mapping.rule = Rule::AsIs;
                        }
                        dialog.focused_mapping = None;
                        true
                    },
                    cx,
                );
            },
            cx,
        )
    };

    let rule_dropdown = {
        let profile = editing.profile.clone();
        Workspace::dropdown(
            ("import-rule", i),
            kind(&mapping.rule),
            vec![offered_rules(target_column(targets, mapping.target))],
            rule_label,
            |_| None,
            move |workspace, rule, _, cx| set_rule(workspace, &profile, i, rule, true, cx),
            cx,
        )
    };
    let profile = editing.profile.clone();
    let format_dropdown = match mapping.rule {
        Rule::Date(format) => Some(Workspace::dropdown(
            ("import-date-format", i),
            format,
            vec![DATE_FORMATS.to_vec()],
            |format| format.label().into(),
            |_| None,
            move |workspace, format, _, cx| {
                set_rule(workspace, &profile, i, Rule::Date(format), false, cx);
            },
            cx,
        )),
        Rule::Number(decimal) => Some(Workspace::dropdown(
            ("import-decimal", i),
            decimal,
            vec![vec![Decimal::Dot, Decimal::Comma]],
            |decimal| match decimal {
                Decimal::Dot => "1,234.56".into(),
                Decimal::Comma => "1.234,56".into(),
            },
            |_| None,
            move |workspace, decimal, _, cx| {
                set_rule(workspace, &profile, i, Rule::Number(decimal), false, cx);
            },
            cx,
        )),
        _ => None,
    };

    let ellipsized = || {
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
    };
    let sample = sample.map(|sample| {
        ellipsized()
            .font_family(fonts(cx).grid.clone())
            .text_size(px(layout::chrome(layout::TEXT_XS)))
            .text_color(t.text_faint)
            .child(one_line(&sample))
    });
    div()
        .id(("import-mapping-row", i))
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .p(px(layout::SPACE_XS))
        .rounded(px(layout::RADIUS_CONTROL))
        .border_1()
        .border_color(if focused {
            gpui::Hsla::from(t.accent)
        } else {
            gpui::transparent_black()
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(ellipsized().text_color(t.text).child(name.to_string()))
                        .children(sample),
                )
                .child(
                    icon(icon::MAPS_TO)
                        .flex_shrink_0()
                        .size(px(layout::chrome(layout::ICON_SIZE)))
                        .text_color(t.text_faint),
                )
                .child(div().flex_1().min_w_0().child(target_dropdown)),
        )
        .children((mapping.target != Target::Skip).then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(t.text_faint)
                        .child("Parse as"),
                )
                .child(div().flex_1().min_w_0().child(rule_dropdown))
                .children(format_dropdown.map(|format| div().flex_1().min_w_0().child(format)))
        }))
        .into_any_element()
}

/// `by_kind` for the Parse as menu, where choosing the kind already chosen
/// keeps its format.
fn set_rule(
    workspace: &mut Workspace,
    profile: &str,
    column: usize,
    rule: Rule,
    by_kind: bool,
    cx: &mut Context<Workspace>,
) {
    workspace.edit_import(
        profile,
        |dialog| {
            let Some(mapping) = dialog.mappings.get_mut(column) else {
                return false;
            };
            let unchanged = match by_kind {
                true => kind(&mapping.rule) == rule,
                false => mapping.rule == rule,
            };
            if unchanged {
                return false;
            }
            mapping.rule = rule;
            dialog.focused_mapping = None;
            true
        },
        cx,
    );
}

fn import_preview(editing: &Editing, cx: &mut Context<Workspace>) -> AnyElement {
    let t = popup_theme(cx);
    let dialog = editing.dialog;
    let view = |id: &'static str, label: &'static str, sql: bool| {
        let profile = editing.profile.clone();
        segment(id, label, dialog.showing_sql == sql, t).on_click(cx.listener(
            move |workspace, _: &ClickEvent, _, cx| {
                workspace.show_import_sql(&profile, sql, cx);
            },
        ))
    };
    let switch = div()
        .flex()
        .gap(px(layout::SPACE_XS))
        .child(view("import-show-rows", "Rows", false))
        .child(view("import-show-sql", "SQL", true));

    let shown = if dialog.showing_sql {
        let more = dialog.plan.as_ref().and_then(|plan| {
            let more = plan.chunks.len().checked_sub(1).filter(|&more| more > 0)?;
            Some(format!(
                "…and {} more {} like it, {} rows in all.",
                group_thousands(more as u64),
                if more == 1 { "statement" } else { "statements" },
                group_thousands(plan.rows() as u64)
            ))
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_SM))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .border_1()
                    .border_color(t.border)
                    .font_family(fonts(cx).editor.clone())
                    .child(
                        Editor::new(&dialog.sql)
                            .readonly(true)
                            .h_full()
                            .appearance(false)
                            .bordered(false)
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            // The query editor's menu, less what a read-only
                            // buffer cannot do.
                            .context_menu(|menu, _, _| {
                                menu.menu("Copy", Box::new(input::Copy))
                                    .menu("Select All", Box::new(input::SelectAll))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(t.text_muted)
                            .children(more),
                    )
                    .child(
                        button(
                            "copy-import-sql",
                            "Copy SQL",
                            Tone::Quiet,
                            Control::Compact,
                            t,
                        )
                        .on_click({
                            let profile = editing.profile.clone();
                            cx.listener(move |workspace, _, _, cx| {
                                workspace.copy_import_sql(&profile, cx);
                            })
                        }),
                    ),
            )
            .into_any_element()
    } else {
        div()
            .flex_1()
            .min_h_0()
            .rounded(px(layout::RADIUS_CONTROL))
            .border_1()
            .border_color(t.border)
            .overflow_hidden()
            .font_family(fonts(cx).grid.clone())
            .text_size(px(layout::grid(layout::BODY_FONT_SIZE)))
            .child(
                DataTable::new(&dialog.preview)
                    .focus_ring(false)
                    .bordered(false)
                    .stripe(false)
                    .with_size(gpui_component::Size::Size(px(layout::grid(
                        gpui_component::Size::Medium.table_row_height().into(),
                    )))),
            )
            .into_any_element()
    };

    div()
        .flex_grow(1.4)
        .flex_basis(px(0.))
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_SM))
        .child(switch)
        .child(shown)
        .into_any_element()
}

fn import_header(dialog: &ImportDialog, cx: &mut Context<Workspace>) -> impl IntoElement {
    let t = popup_theme(cx);
    let workspace = cx.entity().downgrade();
    let (schema, table) = (dialog.schema.clone(), dialog.table.clone());
    let name = dialog
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dialog.path.display().to_string());
    let rows = match &dialog.prepared {
        Some(Ok(prepared)) => Some(row_readout(
            prepared.checked.rows.len(),
            prepared.checked.rows.len(),
        )),
        _ => None,
    };
    let running = matches!(dialog.phase, ImportPhase::Running { .. });
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font_weight(FontWeight::MEDIUM)
                .child(name),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(t.text_muted)
                .child(human_bytes(dialog.bytes.len() as u64)),
        )
        .child(
            button(
                "import-change-file",
                "Change file…",
                Tone::Quiet,
                Control::Standard,
                t,
            )
            .disabled(running)
            .on_click(move |_, window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.prompt_import_file(schema.clone(), table.clone(), window, cx);
                });
            }),
        )
        .child(div().flex_1())
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(t.text_muted)
                .child(format!("{}.{}", dialog.schema, dialog.table)),
        )
        .children(rows.map(|rows| div().flex_shrink_0().child(rows)))
}

/// One child per line: a single child carrying newlines is one run of text to
/// the layout.
fn lines(text: &str) -> impl Iterator<Item = gpui::Div> + '_ {
    text.lines().map(|line| div().child(line.to_string()))
}

/// What stops the import, if anything: the reason the button is disabled.
pub(super) fn import_blocked(dialog: &ImportDialog) -> Option<String> {
    if dialog.committed > 0 {
        return Some(
            "Rows of this file are already in the table, and importing it again would add them \
             twice. Change the file to import the rest."
                .into(),
        );
    }
    if dialog.checking {
        return Some("Checking the file…".into());
    }
    match &dialog.prepared {
        None => Some("Checking the file…".into()),
        Some(Err(message)) => Some(message.clone()),
        Some(Ok(prepared)) => prepared
            .checked
            .problems
            .first()
            .map(|problem| problem.message.clone()),
    }
}

/// "No problems found", or how many there are and where the one the arrows
/// are on sits.
fn problem_summary(problems: &[Problem], at: usize) -> String {
    let Some(problem) = problems.get(at) else {
        return "No problems found".into();
    };
    let count = group_thousands(problems.len() as u64);
    match problem.line {
        Some(line) if at == 0 => format!(
            "{count} {}: first at line {}",
            if problems.len() == 1 {
                "problem"
            } else {
                "problems"
            },
            group_thousands(line as u64)
        ),
        Some(line) => format!(
            "{count} problems: {} of {count} at line {}",
            group_thousands(at as u64 + 1),
            group_thousands(line as u64)
        ),
        None => problem.message.clone(),
    }
}

/// The problem summary, which jumps to the problem it names, and the arrows
/// that step through the rest.
fn problem_navigation(
    profile: &str,
    problems: &[Problem],
    at: usize,
    cx: &mut Context<Workspace>,
) -> gpui::Div {
    let t = popup_theme(cx);
    let summary = div()
        .id("import-problem-summary")
        .child(problem_summary(problems, at));
    let summary = if problems.is_empty() {
        summary
    } else {
        let profile = profile.to_string();
        summary
            .cursor_pointer()
            .hover(|style| style.text_color(t.accent))
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                workspace.jump_to_problem(&profile, at, cx);
            }))
    };
    let step = |id: &'static str, path: &'static str, to: Option<usize>| {
        let profile = profile.to_string();
        icon_button(id, path, Tone::Quiet, Control::Inline, t)
            .disabled(to.is_none())
            .on_click(cx.listener(move |workspace, _, _, cx| {
                if let Some(to) = to {
                    workspace.jump_to_problem(&profile, to, cx);
                }
            }))
    };
    let steps = (problems.len() > 1).then(|| {
        [
            step(
                "import-problem-previous",
                icon::CHEVRON_LEFT,
                at.checked_sub(1),
            ),
            step(
                "import-problem-next",
                icon::CHEVRON_RIGHT,
                (at + 1 < problems.len()).then_some(at + 1),
            ),
        ]
    });
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .child(summary)
        .children(steps.into_iter().flatten())
}

fn import_footer(profile: &str, dialog: &ImportDialog, cx: &mut Context<Workspace>) -> AnyElement {
    let t = popup_theme(cx);
    let cancel_workspace = cx.entity().downgrade();
    let import_workspace = cancel_workspace.clone();
    let summary = match &dialog.prepared {
        Some(Ok(prepared)) => {
            let problems = &prepared.checked.problems;
            let at = dialog.problem_at.min(problems.len().saturating_sub(1));
            Some(problem_navigation(profile, problems, at, cx))
        }
        _ => None,
    };
    let rows = dialog
        .plan
        .as_ref()
        .map(|plan| plan.rows())
        .unwrap_or_default();
    let blocked = import_blocked(dialog);
    let import = button(
        "start-import",
        format!("Import {}", row_readout(rows, rows)),
        // Quiet while disabled, for `run-apply`'s reason: the library's
        // disabled fill dims nothing of a pinned label.
        if blocked.is_some() {
            Tone::Quiet
        } else {
            Tone::Primary
        },
        Control::Standard,
        t,
    )
    .disabled(blocked.is_some())
    .on_click(move |_, _, cx| {
        _ = import_workspace.update(cx, |workspace, cx| workspace.start_import(cx));
    });

    div()
        .flex()
        .items_end()
        .gap(px(layout::SPACE_MD))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .children(summary)
                        .children(
                            dialog
                                .checking
                                .then(|| div().text_color(t.text_faint).child("Checking…")),
                        ),
                )
                .children(dialog.error.as_ref().map(|error| {
                    div()
                        .id("import-error")
                        .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .text_color(t.danger)
                        .children(lines(error))
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_SM))
                .child(
                    button("cancel-import", "Cancel", Tone::Quiet, Control::Standard, t).on_click(
                        move |_, _, cx| {
                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                workspace.close_import(cx);
                            });
                        },
                    ),
                )
                .child(match blocked {
                    Some(why) => import.tooltip(why),
                    None => import,
                }),
        )
        .into_any_element()
}

/// Drawn by hand: gpui-component's progress bar tweens its value.
fn import_progress(dialog: &ImportDialog, cx: &mut Context<Workspace>) -> AnyElement {
    let t = popup_theme(cx);
    let workspace = cx.entity().downgrade();
    let ImportPhase::Running {
        plan,
        run,
        started,
        cancelling,
        ..
    } = &dialog.phase
    else {
        return div().into_any_element();
    };
    let (inserted, total) = (run.inserted(), plan.rows());
    let elapsed = started.elapsed().as_secs();
    let fraction = match total {
        0 => 0.,
        total => inserted as f32 / total as f32,
    };
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    div().h(px(4.)).rounded_full().bg(t.element_active).child(
                        div()
                            .h_full()
                            .w(relative(fraction))
                            .rounded_full()
                            .bg(t.accent),
                    ),
                )
                .child(
                    div()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text_muted)
                        .child(format!(
                            "Inserting {} of {} rows · {}:{:02}",
                            group_thousands(inserted as u64),
                            group_thousands(total as u64),
                            elapsed / 60,
                            elapsed % 60
                        )),
                ),
        )
        .child(
            button(
                "cancel-running-import",
                if *cancelling {
                    "Cancelling…"
                } else {
                    "Cancel"
                },
                Tone::Quiet,
                Control::Standard,
                t,
            )
            .disabled(*cancelling)
            .on_click(move |_, _, cx| {
                _ = workspace.update(cx, |workspace, cx| workspace.cancel_import(cx));
            }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jumped_to_problem_lands_with_context_above_it() {
        assert_eq!(preview_window(3, 1000), 0);
        assert_eq!(preview_window(412, 1000), 402);
        assert_eq!(preview_window(995, 1000), 900);
        assert_eq!(preview_window(5, 50), 0);
    }

    #[test]
    fn the_problem_summary_names_the_one_the_arrows_are_on() {
        let at_line = |line| Problem {
            row: Some(0),
            line: Some(line),
            column: None,
            message: "bad".into(),
        };
        assert_eq!(problem_summary(&[], 0), "No problems found");
        assert_eq!(
            problem_summary(&[at_line(412)], 0),
            "1 problem: first at line 412"
        );
        let three = [at_line(412), at_line(1_500), at_line(2_000)];
        assert_eq!(problem_summary(&three, 0), "3 problems: first at line 412");
        assert_eq!(
            problem_summary(&three, 1),
            "3 problems: 2 of 3 at line 1,500"
        );
        let whole = Problem {
            row: None,
            line: None,
            column: None,
            message: "The file has no rows to import.".into(),
        };
        assert_eq!(
            problem_summary(&[whole], 0),
            "The file has no rows to import."
        );
    }
}
