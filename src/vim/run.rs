use std::ops::Range;

use gpui::{Action, Context, Window};
use gpui_base::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
use gpui_component::input::{self, Rope, RopeExt};

use super::{
    Mode,
    grammar::{
        Command, Count, ModeChange, Modifier, Motion, MotionKeys, Operate, Operator, Paste,
        PasteKeys, Target, TextObject, TextObjectKeys, Visual, VisualSelection,
    },
};
use crate::{session::QueryTab, workspace::Workspace};

/// A command, or one of `Command`'s variants, carried out on the buffer. The
/// only part that touches the editor, through the actions it already has.
pub(super) trait Run {
    fn run(self, tab: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>);
}

impl Run for Command {
    fn run(self, tab: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        match self {
            Command::Motion(motion) => motion.run(tab, window, cx),
            Command::ModeChange(change) => change.run(tab, window, cx),
            Command::Paste(paste) => paste.run(tab, window, cx),
            Command::Operate(operate) => operate.run(tab, window, cx),
        }
    }
}

impl Run for Operate {
    fn run(self, tab: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        self.select(tab, window, cx);
        self.operator.apply(tab, window, cx);
    }
}

/// ponytail: Vim's selection takes in the character under the cursor; the
/// editor's ends before it, so `vd` deletes nothing and `vld` one character.
impl Run for Visual {
    fn run(self, tab: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        match self {
            Visual::Select(VisualSelection::Motion(motion)) => dispatch(
                motion.keys.to_select().as_ref(),
                times(motion.count),
                window,
                cx,
            ),
            Visual::Select(VisualSelection::TextObject(object)) => object.select(1, tab, cx),
            Visual::Operate(operator) => operator.apply(tab, window, cx),
        }
    }
}

/// Moves the cursor.
impl Run for Motion {
    fn run(self, _: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        dispatch(self.keys.to_move().as_ref(), times(self.count), window, cx);
    }
}

impl Run for Paste {
    fn run(self, _: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        match self.keys {
            // ponytail: pastes at the cursor, Vim's `P`, rather than after it,
            // and a yanked line lands mid-line rather than on the line below;
            // the editor's Paste knows nothing of either.
            PasteKeys::After => dispatch(&input::Paste, times(self.count), window, cx),
        }
    }
}

/// Leaves Normal.
impl Run for ModeChange {
    fn run(self, tab: &mut QueryTab, _: &mut Window, cx: &mut Context<Workspace>) {
        match self {
            ModeChange::Insert => tab.vim = Mode::Insert,
            ModeChange::Visual => tab.vim = Mode::Visual,
        }
        cx.notify();
    }
}

impl MotionKeys {
    // ponytail: `w` lands on the end of the next word, Vim's `e`, as the
    // editor's word motion does; Vim's own `w` needs a motion of ours.
    fn to_move(self) -> Box<dyn Action> {
        match self {
            MotionKeys::Left => Box::new(input::MoveLeft),
            MotionKeys::Right => Box::new(input::MoveRight),
            MotionKeys::Up => Box::new(input::MoveUp),
            MotionKeys::Down => Box::new(input::MoveDown),
            MotionKeys::NextWord => Box::new(input::MoveToNextWord),
            MotionKeys::PreviousWordStart => Box::new(input::MoveToPreviousWord),
            MotionKeys::LineStart => Box::new(input::MoveToStartOfLine),
            MotionKeys::LineEnd => Box::new(input::MoveToEndOfLine),
            MotionKeys::BufferStart => Box::new(input::MoveToStart),
            MotionKeys::BufferEnd => Box::new(input::MoveToEnd),
            MotionKeys::NextWORD => todo!(),
            MotionKeys::PreviousWORDStart => todo!(),
            MotionKeys::WordEnd => todo!(),
            MotionKeys::WORDEnd => todo!(),
            MotionKeys::TextStart => todo!(),
            MotionKeys::MoveTill(_) => todo!(),
            MotionKeys::MoveBackTill(_) => todo!(),
            MotionKeys::MoveTo(_) => todo!(),
            MotionKeys::MoveBackTo(_) => todo!(),
            MotionKeys::MatchingPair => todo!(),
        }
    }

    fn to_select(self) -> Box<dyn Action> {
        match self {
            MotionKeys::Left => Box::new(SelectLeft),
            MotionKeys::Right => Box::new(SelectRight),
            // ponytail: Vim's `dj`/`dk` take both whole lines; these select by character,
            // to the same column one line away. Linewise comes with `Target::Line`.
            MotionKeys::Up => Box::new(SelectUp),
            MotionKeys::Down => Box::new(SelectDown),
            MotionKeys::NextWord => Box::new(input::SelectToNextWordEnd),
            MotionKeys::PreviousWordStart => Box::new(input::SelectToPreviousWordStart),
            MotionKeys::LineStart => Box::new(input::SelectToStartOfLine),
            MotionKeys::LineEnd => Box::new(input::SelectToEndOfLine),
            MotionKeys::BufferStart => Box::new(input::SelectToStart),
            MotionKeys::BufferEnd => Box::new(input::SelectToEnd),
            MotionKeys::NextWORD => todo!(),
            MotionKeys::PreviousWORDStart => todo!(),
            MotionKeys::WordEnd => todo!(),
            MotionKeys::WORDEnd => todo!(),
            MotionKeys::TextStart => todo!(),
            MotionKeys::MoveTill(_) => todo!(),
            MotionKeys::MoveBackTill(_) => todo!(),
            MotionKeys::MoveTo(_) => todo!(),
            MotionKeys::MoveBackTo(_) => todo!(),
            MotionKeys::MatchingPair => todo!(),
        }
    }
}

impl Operate {
    /// Selects what the target covers, `count` times over.
    fn select(self, tab: &QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        match self.target {
            Target::Motion(Motion {
                count: motion_count,
                keys,
            }) => dispatch(
                keys.to_select().as_ref(),
                times(self.count) * times(motion_count),
                window,
                cx,
            ),
            Target::TextObject(object) => object.select(times(self.count), tab, cx),
            // ponytail: `cc` takes the newline too, so Insert starts on the
            // line below; Vim's keeps an empty line to type into.
            Target::Line => tab.editor.update(cx, |editor, cx| {
                let text = editor.text();
                let row = text.offset_to_point(editor.cursor()).row;
                let end = (row + times(self.count) as usize).min(text.lines_len());
                let range = lines(text, row..end);
                editor.set_selected_range(range, cx);
            }),
        }
    }
}

impl TextObject {
    /// Selects the object, `count` times over on top of its own count.
    fn select(self, count: u32, tab: &QueryTab, cx: &mut Context<Workspace>) {
        tab.editor.update(cx, |editor, cx| {
            let text = editor.text();
            let row = text.offset_to_point(editor.cursor()).row;
            let count = count * times(self.count);
            let range = match (self.modifier, self.keys) {
                (Modifier::Around, TextObjectKeys::Paragraph) => around_paragraph(text, row, count),
                (Modifier::Inner, TextObjectKeys::Paragraph) => inner_paragraph(text, row, count),
            };
            editor.set_selected_range(range, cx);
        })
    }
}

impl Operator {
    /// Acts on the selection, and says which mode that leaves. Cut, like
    /// Vim's delete, also puts what it took on the clipboard.
    fn apply(self, tab: &mut QueryTab, window: &mut Window, cx: &mut Context<Workspace>) {
        match self {
            Operator::Delete => {
                dispatch(&input::Cut, 1, window, cx);
                tab.vim = Mode::Normal;
            }
            Operator::Yank => {
                dispatch(&input::Copy, 1, window, cx);
                // On a selection this lands on its start, where Vim's yank
                // leaves the cursor.
                dispatch(&input::MoveLeft, 1, window, cx);
                tab.vim = Mode::Normal;
            }
            Operator::Change => {
                dispatch(&input::Cut, 1, window, cx);
                tab.vim = Mode::Insert;
            }
        }
        cx.notify();
    }
}

fn times(count: Option<Count>) -> u32 {
    match count {
        None => 1,
        Some(Count(n)) => n,
    }
}

fn dispatch(action: &dyn Action, times: u32, window: &mut Window, cx: &mut Context<Workspace>) {
    for _ in 0..times {
        window.dispatch_action(action.boxed_clone(), cx);
    }
}

fn inner_paragraph(text: &Rope, row: usize, count: u32) -> Range<usize> {
    lines(text, paragraph_rows(text, row, count))
}

/// Vim's `ap`: twice the paragraphs of `ip`, so a paragraph brings the blank
/// lines after it, and a blank line the paragraph after it.
///
/// ponytail: Vim's last paragraph, with no blank lines after it, takes the
/// ones before it instead; here it takes none, leaving them behind.
fn around_paragraph(text: &Rope, row: usize, count: u32) -> Range<usize> {
    inner_paragraph(text, row, 2 * count)
}

/// The rows of the paragraph around `row` and the `count - 1` after it,
/// where, as in Vim's `ip`, a run of blank lines counts as a paragraph too.
fn paragraph_rows(text: &Rope, row: usize, count: u32) -> Range<usize> {
    let mut first = row;
    while first > 0 && blank(text, first - 1) == blank(text, row) {
        first -= 1;
    }
    let mut next = row;
    for _ in 0..count {
        let run = blank(text, next);
        while next < text.lines_len() && blank(text, next) == run {
            next += 1;
        }
    }
    first..next
}

fn blank(text: &Rope, row: usize) -> bool {
    text.slice_line(row).chars().all(char::is_whitespace)
}

/// Whole lines, so the range ends at the start of the line after the last.
fn lines(text: &Rope, rows: Range<usize>) -> Range<usize> {
    text.line_start_offset(rows.start)..text.line_start_offset(rows.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_paragraph_is_a_run_of_whole_lines() {
        let text = Rope::from("a\nb\n\nc\n");
        assert_eq!(inner_paragraph(&text, 1, 1), 0..4);
        assert_eq!(inner_paragraph(&text, 2, 1), 4..5);
        assert_eq!(inner_paragraph(&text, 0, 2), 0..5);
    }

    #[test]
    fn around_paragraph_takes_the_blank_lines_after() {
        let text = Rope::from("a\n\nb\nc");
        assert_eq!(around_paragraph(&text, 0, 1), 0..3);
        assert_eq!(around_paragraph(&text, 1, 1), 2..6);
        assert_eq!(around_paragraph(&text, 2, 1), 3..6);
    }
}
