use gpui::{Context, Window};

use super::grammar::{
    Command, Modifier, Motion, MotionKeys, Operator, Target, TextObject, TextObjectKeys,
};
use crate::{session::QueryTab, workspace::Workspace};

/// The only part that touches the editor.
pub(super) fn run(
    command: Command,
    tab: &mut QueryTab,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match command {
        // <motion>: move the cursor.
        Command::Motion(Motion { count, keys }) => match keys {
            MotionKeys::Left => todo!(),
            MotionKeys::Right => todo!(),
            MotionKeys::Up => todo!(),
            MotionKeys::Down => todo!(),
            MotionKeys::NextWord => todo!(),
            MotionKeys::PreviousWord => todo!(),
            MotionKeys::LineStart => todo!(),
            MotionKeys::LineEnd => todo!(),
            MotionKeys::BufferStart => todo!(),
            MotionKeys::BufferEnd => todo!(),
        },

        // [<count>] <operator> <target>: first what the target covers, then
        // what the operator does to it.
        Command::Operate {
            count,
            operator,
            target,
        } => {
            match target {
                Target::Motion(Motion {
                    count: motion_count,
                    keys,
                }) => match keys {
                    MotionKeys::Left => todo!(),
                    MotionKeys::Right => todo!(),
                    MotionKeys::Up => todo!(),
                    MotionKeys::Down => todo!(),
                    MotionKeys::NextWord => todo!(),
                    MotionKeys::PreviousWord => todo!(),
                    MotionKeys::LineStart => todo!(),
                    MotionKeys::LineEnd => todo!(),
                    MotionKeys::BufferStart => todo!(),
                    MotionKeys::BufferEnd => todo!(),
                },
                Target::TextObject(TextObject::Bare(keys)) => match keys {
                    TextObjectKeys::Paragraph => todo!(),
                },
                Target::TextObject(TextObject::Modified {
                    count: object_count,
                    modifier,
                    keys,
                }) => match (modifier, keys) {
                    (Modifier::Around, TextObjectKeys::Paragraph) => todo!(),
                    (Modifier::Inner, TextObjectKeys::Paragraph) => todo!(),
                },
                Target::Line => todo!(),
            }

            match operator {
                Operator::Delete => todo!(),
                Operator::Yank => todo!(),
                Operator::Change => todo!(),
            }
        }
    }
}
