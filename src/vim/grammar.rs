// Normal mode's command language, one type per rule:
//
//   count       := {digits}
//   modifier    := a | i
//   motion      := [<count>] <motion-keys>
//   text-object := <text-object-keys>
//                | <modifier> <text-object-keys>
//                | <count> <modifier> <text-object-keys>
//   command     := <motion>
//                | [<count>] <operator> <motion>
//                | [<count>] <operator> <text-object>
//                | [<count>] <operator> <operator>
// https://www.jameswhanlon.com/vim-command-composition.html

/// `{digits}`: how many times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Count(u32);

/// What a command does to what it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operator {
    /// `d`
    Delete,
    /// `y`
    Yank,
    /// `c`
    Change,
}

/// How much of a text object to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modifier {
    /// `a`: the object and what surrounds it.
    Around,
    /// `i`: the object alone.
    Inner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MotionKeys {
    /// `h`
    Left,
    /// `l`
    Right,
    /// `k`
    Up,
    /// `j`
    Down,
    /// `w`
    NextWord,
    /// `b`
    PreviousWord,
    /// `0`
    LineStart,
    /// `$`
    LineEnd,
    /// `gg`
    BufferStart,
    /// `G`
    BufferEnd,
}

/// `[<count>] <motion-keys>`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Motion {
    pub(crate) count: Option<Count>,
    pub(crate) keys: MotionKeys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextObjectKeys {
    /// `p`
    Paragraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextObject {
    /// `<text-object-keys>`
    Bare(TextObjectKeys),
    /// `[<count>] <modifier> <text-object-keys>`: a count only comes with a
    /// modifier.
    Modified {
        count: Option<Count>,
        modifier: Modifier,
        keys: TextObjectKeys,
    },
}

/// What an operator acts on: the alternatives after `<operator>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// `<motion>`
    Motion(Motion),
    /// `<text-object>`
    TextObject(TextObject),
    /// `<operator>` again, the same one: the line.
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// `<motion>`
    Motion(Motion),
    /// `[<count>] <operator> <motion | text-object | operator>`
    Operate {
        count: Option<Count>,
        operator: Operator,
        target: Target,
    },
}

/// What the keys typed so far amount to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Parse {
    /// A whole command, to run.
    Complete(Command),
    /// Valid so far, waiting for more keys.
    More,
    /// Can never become a command, so the keys are dropped.
    Invalid,
}

/// Why a rule did not match: it needs more keys, or no more keys can help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Pending,
    Invalid,
}

/// `count := {digits}`. A count never starts with `0`, which is the
/// line-start motion.
fn count(keys: &str) -> (Option<Count>, &str) {
    todo!()
}

/// `d | y | c`
fn operator(keys: &str) -> Result<(Operator, &str), Stop> {
    todo!()
}

/// `modifier := a | i`
fn modifier(keys: &str) -> Result<(Modifier, &str), Stop> {
    todo!()
}

/// `motion := [<count>] <motion-keys>`
fn motion(keys: &str) -> Result<(Motion, &str), Stop> {
    todo!()
}

/// `text-object := <text-object-keys> | [<count>] <modifier> <text-object-keys>`
fn text_object(keys: &str) -> Result<(TextObject, &str), Stop> {
    todo!()
}

/// `command := <motion> | [<count>] <operator> (<motion> | <text-object> | <operator>)`
///
/// Each alternative is tried in turn and the first to match returns, so the
/// ones after it are never read.
fn command(keys: &str) -> Result<(Command, &str), Stop> {
    todo!()
}

/// `command`, with no keys left over.
pub(super) fn parse(keys: &str) -> Parse {
    match command(keys) {
        Ok((command, "")) => Parse::Complete(command),
        Ok(_) | Err(Stop::Invalid) => Parse::Invalid,
        Err(Stop::Pending) => Parse::More,
    }
}
