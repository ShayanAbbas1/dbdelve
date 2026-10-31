// Normal mode's command language, one type per rule:
//
//   count       := {digits}
//   modifier    := a | i
//   motion      := [<count>] <motion-keys>
//   text-object := <text-object-keys>
//                | <modifier> <text-object-keys>
//                | <count> <modifier> <text-object-keys>
//   mode-change := i
//   shorthand   := x | X | D | C | s | S | Y
//   command     := <motion>
//                | <mode-change>
//                | [<count>] <shorthand>
//                | [<count>] <operator> <motion>
//                | [<count>] <operator> <text-object>
//                | [<count>] <operator> <operator>
// https://www.jameswhanlon.com/vim-command-composition.html

use strum::{EnumIter, IntoEnumIterator};

/// `{digits}`: how many times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Count(pub(super) u32);

/// What a command does to what it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub(crate) enum Operator {
    /// `d`
    Delete,
    /// `y`
    Yank,
    /// `c`
    Change,
}

impl Operator {
    fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Yank => 'y',
            Self::Change => 'c',
        }
    }

    fn from_key(key: char) -> Option<Self> {
        Self::iter().find(|operator| operator.key() == key)
    }
}

/// How much of a text object to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub(crate) enum Modifier {
    /// `a`: the object and what surrounds it.
    Around,
    /// `i`: the object alone.
    Inner,
}

impl Modifier {
    fn key(self) -> char {
        match self {
            Self::Around => 'a',
            Self::Inner => 'i',
        }
    }

    fn from_key(key: char) -> Option<Self> {
        Self::iter().find(|modifier| modifier.key() == key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
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

impl MotionKeys {
    /// A `&str` rather than a `char`, since `gg` is two.
    fn keys(self) -> &'static str {
        match self {
            Self::Left => "h",
            Self::Right => "l",
            Self::Up => "k",
            Self::Down => "j",
            Self::NextWord => "w",
            Self::PreviousWord => "b",
            Self::LineStart => "0",
            Self::LineEnd => "$",
            Self::BufferStart => "gg",
            Self::BufferEnd => "G",
        }
    }
}

/// Which mode Normal leaves for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub(crate) enum ModeChange {
    /// `i`
    Insert,
}

impl ModeChange {
    fn key(self) -> char {
        match self {
            Self::Insert => 'i',
        }
    }

    fn from_key(key: char) -> Option<Self> {
        Self::iter().find(|change| change.key() == key)
    }
}

/// One key standing for an operator and its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub(crate) enum Shorthand {
    /// `x`: `dl`
    DeleteRight,
    /// `X`: `dh`
    DeleteLeft,
    /// `D`: `d$`
    DeleteToLineEnd,
    /// `C`: `c$`
    ChangeToLineEnd,
    /// `s`: `cl`
    ChangeRight,
    /// `S`: `cc`
    ChangeLine,
    /// `Y`: `y$`, as Neovim has it, rather than Vim's `yy`.
    YankToLineEnd,
}

impl Shorthand {
    fn key(self) -> char {
        match self {
            Self::DeleteRight => 'x',
            Self::DeleteLeft => 'X',
            Self::DeleteToLineEnd => 'D',
            Self::ChangeToLineEnd => 'C',
            Self::ChangeRight => 's',
            Self::ChangeLine => 'S',
            Self::YankToLineEnd => 'Y',
        }
    }

    fn from_key(key: char) -> Option<Self> {
        Self::iter().find(|shorthand| shorthand.key() == key)
    }

    /// The `<operator> <target>` it stands for.
    fn expand(self) -> (Operator, Target) {
        let motion = |keys| Target::Motion(Motion { count: None, keys });
        match self {
            Self::DeleteRight => (Operator::Delete, motion(MotionKeys::Right)),
            Self::DeleteLeft => (Operator::Delete, motion(MotionKeys::Left)),
            Self::DeleteToLineEnd => (Operator::Delete, motion(MotionKeys::LineEnd)),
            Self::ChangeToLineEnd => (Operator::Change, motion(MotionKeys::LineEnd)),
            Self::ChangeRight => (Operator::Change, motion(MotionKeys::Right)),
            Self::ChangeLine => (Operator::Change, Target::Line),
            Self::YankToLineEnd => (Operator::Yank, motion(MotionKeys::LineEnd)),
        }
    }
}

/// `[<count>] <motion-keys>`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Motion {
    pub(crate) count: Option<Count>,
    pub(crate) keys: MotionKeys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub(crate) enum TextObjectKeys {
    /// `p`
    Paragraph,
}

impl TextObjectKeys {
    fn key(self) -> char {
        match self {
            Self::Paragraph => 'p',
        }
    }

    fn from_key(key: char) -> Option<Self> {
        Self::iter().find(|object| object.key() == key)
    }
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
    /// `<mode-change>`
    ModeChange(ModeChange),
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

/// `count := {digits}`.
/// A count never starts with `0`, which is the line-start motion.
fn count(keys: &str) -> (Option<Count>, &str) {
    if keys.starts_with('0') {
        return (None, keys);
    }
    let rest = keys.trim_start_matches(|c: char| c.is_ascii_digit());
    let digits = &keys[..keys.len() - rest.len()];
    match digits.parse() {
        Ok(n) => (Some(Count(n)), rest),
        Err(_) => (None, keys),
    }
}

/// `d | y | c`
fn operator(keys: &str) -> Result<(Operator, &str), Stop> {
    let mut chars = keys.chars();
    let operator = match chars.next() {
        None => return Err(Stop::Pending),
        Some(key) => match Operator::from_key(key) {
            None => return Err(Stop::Invalid),
            Some(operator) => operator,
        },
    };
    Ok((operator, chars.as_str()))
}

/// `modifier := a | i`
fn modifier(keys: &str) -> Result<(Modifier, &str), Stop> {
    let mut chars = keys.chars();
    let modifier = match chars.next() {
        None => return Err(Stop::Pending),
        Some(key) => match Modifier::from_key(key) {
            None => return Err(Stop::Invalid),
            Some(modifier) => modifier,
        },
    };
    Ok((modifier, chars.as_str()))
}

/// `mode-change := i`
fn mode_change(keys: &str) -> Result<(ModeChange, &str), Stop> {
    let mut chars = keys.chars();
    let change = match chars.next() {
        None => return Err(Stop::Pending),
        Some(key) => match ModeChange::from_key(key) {
            None => return Err(Stop::Invalid),
            Some(change) => change,
        },
    };
    Ok((change, chars.as_str()))
}

/// `shorthand := x | X | D | C | s | S | Y`
fn shorthand(keys: &str) -> Result<(Shorthand, &str), Stop> {
    let mut chars = keys.chars();
    let shorthand = match chars.next() {
        None => return Err(Stop::Pending),
        Some(key) => match Shorthand::from_key(key) {
            None => return Err(Stop::Invalid),
            Some(shorthand) => shorthand,
        },
    };
    Ok((shorthand, chars.as_str()))
}

/// `motion := [<count>] <motion-keys>`
fn motion(keys: &str) -> Result<(Motion, &str), Stop> {
    let (count, rest) = count(keys);
    for keys in MotionKeys::iter() {
        if let Some(rest) = rest.strip_prefix(keys.keys()) {
            return Ok((Motion { count, keys }, rest));
        }
    }
    // What was typed is the start of some motion's keys, such as `g` of `gg`.
    if MotionKeys::iter().any(|keys| keys.keys().starts_with(rest)) {
        Err(Stop::Pending)
    } else {
        Err(Stop::Invalid)
    }
}

/// `<text-object-keys>`
fn text_object_keys(keys: &str) -> Result<(TextObjectKeys, &str), Stop> {
    let mut chars = keys.chars();
    let object_keys = match chars.next() {
        None => return Err(Stop::Pending),
        Some(key) => match TextObjectKeys::from_key(key) {
            None => return Err(Stop::Invalid),
            Some(object_keys) => object_keys,
        },
    };
    Ok((object_keys, chars.as_str()))
}

/// `text-object := <text-object-keys> | [<count>] <modifier> <text-object-keys>`
fn text_object(keys: &str) -> Result<(TextObject, &str), Stop> {
    // <text-object-keys>
    let not_bare = match text_object_keys(keys) {
        Ok((object_keys, rest)) => return Ok((TextObject::Bare(object_keys), rest)),
        Err(stop) => stop,
    };

    // | [<count>] <modifier> <text-object-keys>
    let (count, rest) = count(keys);
    let (modifier, rest) = match modifier(rest) {
        Ok(found) => found,
        Err(not_modifier) => return Err(none_matched([not_bare, not_modifier])),
    };
    let (object_keys, rest) = text_object_keys(rest)?;
    Ok((
        TextObject::Modified {
            count,
            modifier,
            keys: object_keys,
        },
        rest,
    ))
}

/// The grammar's `|` when no alternative matched: wait for more keys if any of
/// them still could.
fn none_matched<const N: usize>(stops: [Stop; N]) -> Stop {
    if stops.contains(&Stop::Pending) {
        Stop::Pending
    } else {
        Stop::Invalid
    }
}

/// `command := <motion> | <mode-change> | [<count>] <shorthand> | [<count>] <operator> (<motion> | <text-object> | <operator>)`
///
/// Each alternative is tried in turn and the first to match returns, so the
/// ones after it are never read.
fn command(keys: &str) -> Result<(Command, &str), Stop> {
    // <motion>
    let not_motion = match motion(keys) {
        Ok((motion, rest)) => return Ok((Command::Motion(motion), rest)),
        Err(stop) => stop,
    };

    // | <mode-change>
    let not_mode_change = match mode_change(keys) {
        Ok((change, rest)) => return Ok((Command::ModeChange(change), rest)),
        Err(stop) => stop,
    };

    // Both alternatives left start with `[<count>]`.
    let (count, rest) = count(keys);

    // | [<count>] <shorthand>
    let not_shorthand = match shorthand(rest) {
        Ok((shorthand, rest)) => {
            let (operator, target) = shorthand.expand();
            return Ok((
                Command::Operate {
                    count,
                    operator,
                    target,
                },
                rest,
            ));
        }
        Err(stop) => stop,
    };

    // | [<count>] <operator> (<motion> | <text-object> | <operator>)
    let (first, rest) = match operator(rest) {
        Ok(found) => found,
        Err(not_operator) => {
            return Err(none_matched([
                not_motion,
                not_mode_change,
                not_shorthand,
                not_operator,
            ]));
        }
    };
    let operate = |target| Command::Operate {
        count,
        operator: first,
        target,
    };
    let not_motion = match motion(rest) {
        Ok((motion, rest)) => return Ok((operate(Target::Motion(motion)), rest)),
        Err(stop) => stop,
    };
    let not_object = match text_object(rest) {
        Ok((object, rest)) => return Ok((operate(Target::TextObject(object)), rest)),
        Err(stop) => stop,
    };
    // The second operator must be the first again.
    match operator(rest) {
        Ok((second, rest)) if second == first => Ok((operate(Target::Line), rest)),
        Ok(_) => Err(none_matched([not_motion, not_object])),
        Err(not_operator) => Err(none_matched([not_motion, not_object, not_operator])),
    }
}

/// `command`, with no keys left over.
pub(super) fn parse(keys: &str) -> Parse {
    match command(keys) {
        Ok((command, "")) => Parse::Complete(command),
        Ok(_) | Err(Stop::Invalid) => Parse::Invalid,
        Err(Stop::Pending) => Parse::More,
    }
}
